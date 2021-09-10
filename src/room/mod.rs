//! Room buffer module.
//!
//! This module implements creates buffers that processes and prints out all the
//! user visible events
//!
//! Care should be taken when handling events. Events can be state events or
//! timeline events and they can come from a sync response or from a room
//! messages response.
//!
//! Events coming from a sync response and are part of the timeline need to be
//! printed out and they need to change the buffer state (e.g. when someone
//! joins, they need to be added to the nicklist).
//!
//! Events coming from a sync response and are part of the room state only need
//! to change the buffer state.
//!
//! Events coming from a room messages response, meaning they are old events,
//! should never change the room state. They only should be printed out.
//!
//! Care should be taken to model this in a way that event formatting methods
//! are pure functions so they can be reused e.g. if we print messages that
//! we're sending ourselves before we receive them in a sync response, or if we
//! decrypt a previously undecryptable event.

mod members;

use members::Members;
pub use members::WeechatRoomMember;
use tracing::{debug, trace};

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    ops::Deref,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard,
    },
};

use futures::executor::block_on;

use unicode_segmentation::UnicodeSegmentation;
use url::Url;

use matrix_sdk::{
    async_trait,
    deserialized_responses::AmbiguityChange,
    room::Joined,
    ruma::{
        events::{
            room::{
                member::MemberEventContent,
                message::{
                    MessageEventContent, MessageType, TextMessageEventContent,
                },
                redaction::SyncRedactionEvent,
            },
            AnyMessageEventContent, AnyRedactedSyncMessageEvent, AnyRoomEvent,
            AnySyncMessageEvent, AnySyncRoomEvent, AnySyncStateEvent,
            SyncMessageEvent, SyncStateEvent,
        },
        EventId, MilliSecondsSinceUnixEpoch, RoomAliasId, RoomId, UserId,
    },
    uuid::Uuid,
    StoreError,
};

use weechat::{
    buffer::{
        Buffer, BufferBuilderAsync, BufferHandle, BufferInputCallbackAsync,
        BufferLine, LineData,
    },
    Weechat,
};

use crate::{
    config::{Config, RedactionStyle},
    connection::Connection,
    render::{Render, RenderedEvent},
    utils::{Edit, ToTag},
    PLUGIN_NAME,
};

#[derive(Clone)]
pub struct RoomHandle {
    inner: MatrixRoom,
    buffer_handle: BufferHandle,
}

#[derive(Debug, Clone)]
pub enum PrevBatch {
    Forward(String),
    Backwards(String),
}

impl Deref for RoomHandle {
    type Target = MatrixRoom;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Clone, Debug)]
struct IntMutex {
    inner: Rc<Mutex<Rc<AtomicBool>>>,
    locked: Rc<AtomicBool>,
}

struct IntMutexGuard<'a> {
    inner: MutexGuard<'a, Rc<AtomicBool>>,
}

impl<'a> Drop for IntMutexGuard<'a> {
    fn drop(&mut self) {
        self.inner.store(false, Ordering::SeqCst)
    }
}

impl IntMutex {
    fn new() -> Self {
        let locked = Rc::new(AtomicBool::from(false));
        let inner = Rc::new(Mutex::new(locked.clone()));

        Self { inner, locked }
    }

    fn locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }

    fn try_lock(&self) -> Result<IntMutexGuard<'_>, ()> {
        match self.inner.try_lock() {
            Ok(guard) => {
                guard.store(true, Ordering::SeqCst);

                Ok(IntMutexGuard { inner: guard })
            }
            Err(_) => Err(()),
        }
    }
}

#[derive(Clone)]
pub struct MatrixRoom {
    homeserver: Rc<Url>,
    room_id: Rc<RoomId>,
    own_user_id: Rc<UserId>,
    room: Joined,
    buffer: Rc<RefCell<Option<BufferHandle>>>,

    config: Rc<RefCell<Config>>,
    connection: Rc<RefCell<Option<Connection>>>,

    messages_in_flight: IntMutex,
    prev_batch: Rc<RefCell<Option<PrevBatch>>>,

    outgoing_messages: MessageQueue,

    members: Members,
}

#[derive(Debug, Clone, Default)]
pub struct MessageQueue {
    queue: Rc<RefCell<HashMap<Uuid, (bool, MessageEventContent)>>>,
}

impl MessageQueue {
    fn new() -> Self {
        Self {
            queue: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn add(&self, uuid: Uuid, content: MessageEventContent) {
        self.queue.borrow_mut().insert(uuid, (false, content));
    }

    fn add_with_echo(&self, uuid: Uuid, content: MessageEventContent) {
        self.queue.borrow_mut().insert(uuid, (true, content));
    }

    fn remove(&self, uuid: Uuid) -> Option<(bool, MessageEventContent)> {
        self.queue.borrow_mut().remove(&uuid)
    }
}

impl RoomHandle {
    pub fn new(
        server_name: &str,
        connection: &Rc<RefCell<Option<Connection>>>,
        config: Rc<RefCell<Config>>,
        room: Joined,
        homeserver: Url,
        room_id: RoomId,
        own_user_id: &UserId,
    ) -> Self {
        let members = Members::new(room.clone());

        let own_nick = block_on(room.get_member_no_sync(own_user_id))
            .ok()
            .flatten()
            .map(|m| m.name().to_owned())
            .unwrap_or_else(|| own_user_id.localpart().to_owned());

        let room = MatrixRoom {
            homeserver: Rc::new(homeserver),
            room_id: Rc::new(room_id.clone()),
            connection: connection.clone(),
            config,
            prev_batch: Rc::new(RefCell::new(
                room.last_prev_batch().map(PrevBatch::Backwards),
            )),
            own_user_id: Rc::new(own_user_id.to_owned()),
            members: members.clone(),
            buffer: members.buffer,
            outgoing_messages: MessageQueue::new(),
            messages_in_flight: IntMutex::new(),
            room,
        };

        let buffer_name = format!("{}.{}", server_name, room_id);

        let buffer_handle = BufferBuilderAsync::new(&buffer_name)
            .input_callback(room.clone())
            .close_callback(|_weechat: &Weechat, _buffer: &Buffer| {
                // TODO remove the roombuffer from the server here.
                // TODO leave the room if the plugin isn't unloading.
                Ok(())
            })
            .build()
            .expect("Can't create new room buffer");

        let buffer = buffer_handle
            .upgrade()
            .expect("Can't upgrade newly created buffer");

        buffer
            .add_nicklist_group(
                "000|o",
                "weechat.color.nicklist_group",
                true,
                None,
            )
            .expect("Can't create nicklist group");
        buffer
            .add_nicklist_group(
                "001|h",
                "weechat.color.nicklist_group",
                true,
                None,
            )
            .expect("Can't create nicklist group");
        buffer
            .add_nicklist_group(
                "002|v",
                "weechat.color.nicklist_group",
                true,
                None,
            )
            .expect("Can't create nicklist group");
        buffer
            .add_nicklist_group(
                "999|...",
                "weechat.color.nicklist_group",
                true,
                None,
            )
            .expect("Can't create nicklist group");

        buffer.enable_nicklist();
        buffer.disable_nicklist_groups();
        buffer.enable_multiline();

        buffer.set_localvar("server", server_name);
        buffer.set_localvar("nick", &own_nick);
        buffer.set_localvar("domain", room.room_id().server_name().as_str());
        buffer.set_localvar("room_id", room.room_id().as_str());
        if room.is_direct() {
            buffer.set_localvar("type", "private")
        } else {
            buffer.set_localvar("type", "channel")
        }

        if let Some(alias) = room.alias() {
            buffer.set_localvar("alias", alias.as_str());
        }

        *room.members.buffer.borrow_mut() = Some(buffer_handle.clone());

        Self {
            inner: room,
            buffer_handle,
        }
    }

    pub async fn restore(
        server_name: &str,
        room: Joined,
        connection: &Rc<RefCell<Option<Connection>>>,
        config: Rc<RefCell<Config>>,
        homeserver: Url,
    ) -> Result<Self, StoreError> {
        let room_clone = room.clone();
        let room_id = room.room_id();
        let own_user_id = room.own_user_id();
        let prev_batch = room.last_prev_batch();

        let room_buffer = Self::new(
            server_name,
            connection,
            config,
            room_clone,
            homeserver,
            room_id.clone(),
            own_user_id,
        );

        debug!("Restoring room {}", room.room_id());

        let matrix_members = room.joined_user_ids().await?;

        for user_id in matrix_members {
            trace!("Restoring member {}", user_id);
            room_buffer.members.restore_member(&user_id).await;
        }

        *room_buffer.prev_batch.borrow_mut() =
            prev_batch.map(PrevBatch::Forward);

        room_buffer.update_buffer_name();
        room_buffer.set_topic();

        Ok(room_buffer)
    }
}

#[async_trait(?Send)]
impl BufferInputCallbackAsync for MatrixRoom {
    async fn callback(&mut self, _: BufferHandle, input: String) {
        let content = if self.config.borrow().input().markdown_input() {
            MessageEventContent::new(MessageType::Text(
                TextMessageEventContent::markdown(input),
            ))
        } else {
            MessageEventContent::new(MessageType::Text(
                TextMessageEventContent::plain(input),
            ))
        };

        self.send_message(content).await;
    }
}

impl MatrixRoom {
    pub fn is_encrypted(&self) -> bool {
        self.room.is_encrypted()
    }

    pub fn is_public(&self) -> bool {
        self.room.is_public()
    }

    pub fn is_direct(&self) -> bool {
        self.room.is_direct()
    }

    pub fn alias(&self) -> Option<RoomAliasId> {
        self.room.canonical_alias()
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn buffer_handle(&self) -> BufferHandle {
        self.buffer
            .borrow()
            .as_ref()
            .expect("Room struct wasn't initialized properly")
            .clone()
    }

    fn print_rendered_event(&self, rendered: RenderedEvent) {
        let buffer = self.buffer_handle();

        if let Ok(buffer) = buffer.upgrade() {
            for line in rendered.content.lines {
                let message = format!("{}{}", &rendered.prefix, &line.message);
                let tags: Vec<&str> =
                    line.tags.iter().map(|t| t.as_str()).collect();
                buffer.print_date_tags(
                    rendered.message_timestamp,
                    &tags,
                    &message,
                )
            }
        }
    }

    async fn redact_event(&self, event: &SyncRedactionEvent) {
        let buffer_handle = self.buffer_handle();

        let buffer = if let Ok(b) = buffer_handle.upgrade() {
            b
        } else {
            return;
        };

        // TODO remove this unwrap.
        let redacter = self.members.get(&event.sender).await.unwrap();

        let event_id_tag =
            Cow::from(format!("{}_id_{}", PLUGIN_NAME, event.redacts));
        let tag = Cow::from("matrix_redacted");

        let reason = if let Some(r) = &event.content.reason {
            format!(", reason: {}", r)
        } else {
            "".to_owned()
        };
        let redaction_message = format!(
            "{}<{}Message redacted by: {}{}{}>{}",
            Weechat::color("chat_delimiters"),
            Weechat::color("logger.color.backlog_line"),
            redacter.nick(),
            reason,
            Weechat::color("chat_delimiters"),
            Weechat::color("reset"),
        );

        let redaction_style = self.config.borrow().look().redaction_style();

        let predicate = |l: &BufferLine| {
            let tags = l.tags();
            tags.contains(&event_id_tag)
                && !tags.contains(&Cow::from("matrix_redacted"))
        };

        let strike_through = |string: Cow<str>| {
            Weechat::remove_color(&string)
                .graphemes(true)
                .map(|g| format!("{}\u{0336}", g))
                .collect::<Vec<String>>()
                .join("")
        };

        let redact_first_line = |message: Cow<str>| match redaction_style {
            RedactionStyle::Delete => redaction_message.clone(),
            RedactionStyle::Notice => {
                format!("{} {}", message, redaction_message)
            }
            RedactionStyle::StrikeThrough => {
                format!("{} {}", strike_through(message), redaction_message)
            }
        };

        let redact_string = |message: Cow<str>| match redaction_style {
            RedactionStyle::Delete => redaction_message.clone(),
            RedactionStyle::Notice => {
                format!("{} {}", message, redaction_message)
            }
            RedactionStyle::StrikeThrough => strike_through(message),
        };

        fn modify_line<F>(line: BufferLine, tag: Cow<str>, redaction_func: F)
        where
            F: Fn(Cow<str>) -> String,
        {
            let message = line.message();
            let new_message = redaction_func(message);

            let mut tags = line.tags();
            tags.push(tag);
            let tags: Vec<&str> = tags.iter().map(|t| t.as_ref()).collect();

            line.set_message(&new_message);
            line.set_tags(&tags);
        }

        let mut lines = buffer.lines();
        let first_line = lines.rfind(predicate);

        if let Some(line) = first_line {
            modify_line(line, tag.clone(), redact_first_line);
        } else {
            return;
        }

        while let Some(line) = lines.next_back().filter(predicate) {
            modify_line(line, tag.clone(), redact_string);
        }
    }

    async fn render_message_content(
        &self,
        event_id: &EventId,
        send_time: &MilliSecondsSinceUnixEpoch,
        sender: &WeechatRoomMember,
        content: &AnyMessageEventContent,
    ) -> Option<RenderedEvent> {
        use AnyMessageEventContent::*;
        use MessageType::*;

        let rendered = match content {
            RoomEncrypted(c) => {
                c.render_with_prefix(send_time, event_id, sender, &())
            }
            RoomMessage(c) => match &c.msgtype {
                Text(c) => {
                    c.render_with_prefix(send_time, event_id, sender, &())
                }
                Emote(c) => {
                    c.render_with_prefix(send_time, event_id, &sender, &sender)
                }
                Notice(c) => {
                    c.render_with_prefix(send_time, event_id, &sender, &sender)
                }
                ServerNotice(c) => {
                    c.render_with_prefix(send_time, event_id, &sender, &sender)
                }
                Location(c) => {
                    c.render_with_prefix(send_time, event_id, &sender, &sender)
                }
                Audio(c) => c.render_with_prefix(
                    send_time,
                    event_id,
                    &sender,
                    &self.homeserver,
                ),
                Video(c) => c.render_with_prefix(
                    send_time,
                    event_id,
                    &sender,
                    &self.homeserver,
                ),
                File(c) => c.render_with_prefix(
                    send_time,
                    event_id,
                    &sender,
                    &self.homeserver,
                ),
                Image(c) => c.render_with_prefix(
                    send_time,
                    event_id,
                    &sender,
                    &self.homeserver,
                ),
                _ => return None,
            },
            _ => return None,
        };

        Some(rendered)
    }

    async fn render_sync_message(
        &self,
        event: &AnySyncMessageEvent,
    ) -> Option<RenderedEvent> {
        // TODO remove this expect.
        let sender =
            self.members.get(event.sender()).await.expect(
                "Rendering a message but the sender isn't in the nicklist",
            );

        let send_time = event.origin_server_ts();
        self.render_message_content(
            event.event_id(),
            send_time,
            &sender,
            &event.content(),
        )
        .await
        .map(|r| {
            // TODO the tags are different if the room is a DM.
            if sender.user_id() == &*self.own_user_id {
                r.add_self_tags()
            } else {
                r.add_msg_tags()
            }
        })
    }

    // Add the content of the message to our outgoing messag queue and print out
    // a local echo line if local echo is enabled.
    async fn queue_outgoing_message(
        &self,
        uuid: Uuid,
        content: &MessageEventContent,
    ) {
        if self.config.borrow().look().local_echo() {
            if let MessageType::Text(c) = &content.msgtype {
                let sender =
                    self.members.get(&self.own_user_id).await.unwrap_or_else(
                        || panic!("No own member {}", self.own_user_id),
                    );

                let local_echo = c
                    .render_with_prefix_for_echo(&sender, uuid, &())
                    .add_self_tags();
                self.print_rendered_event(local_echo);

                self.outgoing_messages.add_with_echo(uuid, content.clone());
            } else {
                self.outgoing_messages.add(uuid, content.clone());
            }
        } else {
            self.outgoing_messages.add(uuid, content.clone());
        }
    }

    /// Send the given content to the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content that should be sent to the server.
    ///
    /// # Examples
    ///
    /// ```
    /// let content = MessageEventContent::Text(TextMessageEventContent {
    ///     body: "Hello world".to_owned(),
    ///     formatted: None,
    ///     relates_to: None,
    /// });
    /// let content = AnyMessageEventContent::RoomMessage(content);
    ///
    /// buffer.send_message(content).await
    /// ```
    pub async fn send_message(&self, content: MessageEventContent) {
        let uuid = Uuid::new_v4();

        let connection = self.connection.borrow().clone();

        if let Some(c) = connection {
            self.queue_outgoing_message(uuid, &content).await;
            match c
                .send_message(
                    self.room().clone(),
                    AnyMessageEventContent::RoomMessage(content),
                    Some(uuid),
                )
                .await
            {
                Ok(r) => {
                    self.handle_outgoing_message(uuid, &r.event_id).await;
                }
                Err(_e) => {
                    // TODO print out an error, remember to modify the local
                    // echo line if there is one.
                    self.outgoing_messages.remove(uuid);
                }
            }
        } else if let Ok(buffer) = self.buffer_handle().upgrade() {
            buffer.print("Error not connected");
        }
    }

    /// Send out a typing notice.
    ///
    /// This will send out a typing notice or reset the one in progress, if
    /// needed. It will make sure that only one typing notice request is in
    /// flight at a time.
    ///
    /// Typing notices are sent out only if we have more than 4 letters in the
    /// input and the input isn't a command.
    ///
    /// If the input is empty the typing notice is disabled.
    pub fn update_typing_notice(&self) {
        let buffer_handle = self.buffer_handle();

        let buffer = if let Ok(b) = buffer_handle.upgrade() {
            b
        } else {
            return;
        };

        let input = buffer.input();

        if input.starts_with('/') && !input.starts_with("//") {
            // Don't send typing notices for commands.
            return;
        }

        let connection = self.connection.clone();
        let room = self.room().clone();

        let send = |typing: bool| async move {
            let connection = connection.borrow().clone();

            if let Some(connection) = connection {
                let _ = connection.send_typing_notice(room, typing).await;
            };
        };

        if input.len() < 4 {
            // If we have an active typing notice and our input is short, e.g.
            // we removed the input set the typing notice to false.
            Weechat::spawn(send(false)).detach();
        } else if input.len() >= 4 {
            // If we have some valid input and no active typing notice, send
            // one out.
            Weechat::spawn(send(true)).detach();
        }
    }

    pub fn is_busy(&self) -> bool {
        self.messages_in_flight.locked()
    }

    pub fn reset_prev_batch(&self) {
        // TODO we'll want to be able to scroll up again after we clear the
        // buffer.
        *self.prev_batch.borrow_mut() = None;
    }

    pub async fn get_messages(&self) {
        let messages_lock = self.messages_in_flight.clone();

        let connection = self.connection.borrow().as_ref().cloned();

        let prev_batch =
            if let Some(p) = self.prev_batch.borrow().as_ref().cloned() {
                p
            } else {
                return;
            };

        let guard = if let Ok(l) = messages_lock.try_lock() {
            l
        } else {
            return;
        };

        Weechat::bar_item_update("buffer_modes");
        Weechat::bar_item_update("matrix_modes");

        if let Some(connection) = connection {
            let room = self.room().clone();

            if let Ok(r) = connection.room_messages(room, prev_batch).await {
                for event in r.chunk.iter().filter_map(|e| e.deserialize().ok())
                {
                    self.handle_room_event(&event).await;
                }

                let mut prev_batch = self.prev_batch.borrow_mut();

                if let Some(PrevBatch::Forward(t)) = prev_batch.as_ref() {
                    *prev_batch = Some(PrevBatch::Backwards(t.to_owned()));
                    self.sort_messages();
                } else if r.chunk.is_empty() {
                    *prev_batch = None;
                } else {
                    *prev_batch = r.end.map(PrevBatch::Backwards);
                    self.sort_messages();
                }
            }
        }

        drop(guard);

        Weechat::bar_item_update("buffer_modes");
        Weechat::bar_item_update("matrix_modes");
    }

    fn sort_messages(&self) {
        struct LineCopy {
            date: i64,
            date_printed: i64,
            tags: Vec<String>,
            prefix: String,
            message: String,
        }

        impl<'a> From<BufferLine<'a>> for LineCopy {
            fn from(line: BufferLine) -> Self {
                Self {
                    date: line.date(),
                    date_printed: line.date_printed(),
                    message: line.message().to_string(),
                    prefix: line.prefix().to_string(),
                    tags: line.tags().iter().map(|t| t.to_string()).collect(),
                }
            }
        }

        // TODO update the highlight once Weechat starts supporting it.
        if let Ok(buffer) = self.buffer_handle().upgrade() {
            let mut lines: Vec<LineCopy> =
                buffer.lines().map(|l| l.into()).collect();
            lines.sort_by_key(|l| l.date);

            for (line, new) in buffer.lines().zip(lines.drain(..)) {
                let tags =
                    new.tags.iter().map(|t| t.as_str()).collect::<Vec<&str>>();
                let data = LineData {
                    prefix: Some(&new.prefix),
                    message: Some(&new.message),
                    date: Some(new.date),
                    date_printed: Some(new.date_printed),
                    tags: Some(&tags),
                };
                line.update(data)
            }
        }
    }

    /// Replace the local echo of an event with a fully rendered one.
    fn replace_local_echo(
        &self,
        uuid: Uuid,
        buffer: &Buffer,
        rendered: RenderedEvent,
    ) {
        let uuid_tag = Cow::from(format!("matrix_echo_{}", uuid.to_string()));
        let line_contains_uuid = |l: &BufferLine| l.tags().contains(&uuid_tag);

        let mut lines = buffer.lines();
        let mut current_line = lines.rfind(line_contains_uuid);

        // We go in reverse order here since we also use rfind(). We got from
        // the bottom of the buffer to the top since we're expecting these
        // lines to be freshly printed and thus at the bottom.
        let mut line_num = rendered.content.lines.len();

        while let Some(line) = &current_line {
            line_num -= 1;
            let rendered_line = &rendered.content.lines[line_num];

            line.set_message(&rendered_line.message);
            current_line = lines.next_back().filter(line_contains_uuid);
        }
    }

    async fn handle_outgoing_message(&self, uuid: Uuid, event_id: &EventId) {
        if let Some((echo, content)) = self.outgoing_messages.remove(uuid) {
            let event = SyncMessageEvent {
                sender: (&*self.own_user_id).clone(),
                origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
                event_id: event_id.clone(),
                content,
                unsigned: Default::default(),
            };

            let event = AnySyncMessageEvent::RoomMessage(event);

            let rendered = self
                .render_sync_message(&event)
                .await
                .expect("Sent out an event that we don't know how to render");

            if let Ok(buffer) = self.buffer_handle().upgrade() {
                if echo {
                    self.replace_local_echo(uuid, &buffer, rendered);
                } else {
                    self.print_rendered_event(rendered);
                }
            }
        }
    }

    fn set_topic(&self) {
        if let Ok(buffer) = self.buffer_handle().upgrade() {
            buffer.set_title(&self.room().topic().unwrap_or_default());
        }
    }

    fn set_alias(&self) {
        if let Some(alias) = self.alias() {
            if let Ok(b) = self.buffer_handle().upgrade() {
                b.set_localvar("alias", alias.as_str());
            }
        }
    }

    fn update_buffer_name(&self) {
        self.members.update_buffer_name();
    }

    fn replace_edit(
        &self,
        event_id: &EventId,
        sender: &UserId,
        event: RenderedEvent,
    ) {
        if let Ok(buffer) = self.buffer_handle().upgrade() {
            let sender_tag = Cow::from(sender.to_tag());
            let event_id_tag = Cow::from(event_id.to_tag());

            let lines: Vec<BufferLine> = buffer
                .lines()
                .filter(|l| l.tags().contains(&event_id_tag))
                .collect();

            if lines
                .get(0)
                .map(|l| l.tags().contains(&sender_tag))
                .unwrap_or(false)
            {
                self.replace_event_helper(&buffer, lines, event);
            }
        }
    }

    fn replace_event_helper(
        &self,
        buffer: &Buffer,
        lines: Vec<BufferLine<'_>>,
        event: RenderedEvent,
    ) {
        use std::cmp::Ordering;
        let date = lines.get(0).map(|l| l.date()).unwrap_or_default();

        for (line, new) in lines.iter().zip(event.content.lines.iter()) {
            let tags: Vec<&str> = new.tags.iter().map(|t| t.as_str()).collect();
            let data = LineData {
                // Our prefixes always come with a \t character, but when we
                // replace stuff we're able to replace the prefix and the
                // message separately, so trim the whitespace in the prefix.
                prefix: Some(event.prefix.trim_end()),
                message: Some(&new.message),
                tags: Some(&tags),
                ..Default::default()
            };

            line.update(data);
        }

        match lines.len().cmp(&event.content.lines.len()) {
            Ordering::Greater => {
                for line in &lines[event.content.lines.len()..] {
                    line.set_message("");
                }
            }
            Ordering::Less => {
                for line in &event.content.lines[lines.len()..] {
                    let message = format!("{}{}", &event.prefix, &line.message);
                    let tags: Vec<&str> =
                        line.tags.iter().map(|t| t.as_str()).collect();
                    buffer.print_date_tags(date, &tags, &message)
                }

                self.sort_messages()
            }
            Ordering::Equal => (),
        }
    }

    async fn handle_edits(&self, event: &AnySyncMessageEvent) {
        // TODO remove this expect.
        let sender =
            self.members.get(event.sender()).await.expect(
                "Rendering a message but the sender isn't in the nicklist",
            );

        if let Some((event_id, content)) = event.get_edit() {
            let send_time = event.origin_server_ts();

            if let Some(rendered) = self
                .render_message_content(
                    event_id,
                    send_time,
                    &sender,
                    &AnyMessageEventContent::RoomMessage(content.clone()),
                )
                .await
                .map(|r| {
                    // TODO the tags are different if the room is a DM.
                    if sender.user_id() == &*self.own_user_id {
                        r.add_self_tags()
                    } else {
                        r.add_msg_tags()
                    }
                })
            {
                self.replace_edit(event_id, event.sender(), rendered);
            }
        }
    }

    async fn handle_room_message(&self, event: &AnySyncMessageEvent) {
        // If the event has a transaction id it's an event that we sent out
        // ourselves, the content will be in the outgoing message queue and it
        // may have been printed out as a local echo.
        if let Some(id) = &event.unsigned().transaction_id {
            if let Ok(id) = Uuid::parse_str(id) {
                self.handle_outgoing_message(id, event.event_id()).await;
                return;
            }
        }

        if let AnySyncMessageEvent::RoomRedaction(r) = event {
            self.redact_event(r).await;
        } else if event.is_edit() {
            self.handle_edits(event).await;
        } else if let Some(rendered) = self.render_sync_message(event).await {
            self.print_rendered_event(rendered);
        }
    }

    async fn handle_redacted_events(
        &self,
        event: &AnyRedactedSyncMessageEvent,
    ) {
        use AnyRedactedSyncMessageEvent::*;

        if let RoomMessage(e) = event {
            // TODO remove those expects and unwraps.
            let redacter =
                &e.unsigned.redacted_because.as_ref().unwrap().sender;
            let redacter = self.members.get(redacter).await.expect(
                "Rendering a message but the sender isn't in the nicklist",
            );
            let sender = self.members.get(&e.sender).await.expect(
                "Rendering a message but the sender isn't in the nicklist",
            );
            let rendered = e.render_with_prefix(
                &e.origin_server_ts,
                event.event_id(),
                &sender,
                &redacter,
            );

            self.print_rendered_event(rendered);
        }
    }

    pub async fn handle_membership_event(
        &self,
        event: &SyncStateEvent<MemberEventContent>,
        state_event: bool,
        ambiguity_change: Option<&AmbiguityChange>,
    ) {
        self.members
            .handle_membership_event(event, state_event, ambiguity_change)
            .await
    }

    fn set_prev_batch(&self) {
        if let Ok(buffer) = self.buffer_handle().upgrade() {
            if buffer.num_lines() == 0 {
                *self.prev_batch.borrow_mut() =
                    self.room.last_prev_batch().map(PrevBatch::Backwards);
            }
        }
    }

    pub async fn handle_sync_room_event(&self, event: AnySyncRoomEvent) {
        self.set_prev_batch();

        match &event {
            AnySyncRoomEvent::Message(message) => {
                self.handle_room_message(message).await
            }
            AnySyncRoomEvent::RedactedMessage(e) => {
                self.handle_redacted_events(e).await
            }
            // We don't print out redacted state events for now.
            AnySyncRoomEvent::RedactedState(_) => (),
            AnySyncRoomEvent::State(event) => {
                self.handle_sync_state_event(event, false).await
            }
        }
    }

    pub async fn handle_room_event(&self, event: &AnyRoomEvent) {
        match &event {
            AnyRoomEvent::Message(event) => {
                // Only print out historical events if they aren't edits of
                // other events.
                if !event.is_edit() {
                    let sender = self.members.get(event.sender()).await.expect(
                        "Rendering a message but the sender isn't in the nicklist",
                    );

                    let send_time = event.origin_server_ts();

                    if let Some(rendered) = self
                        .render_message_content(
                            event.event_id(),
                            send_time,
                            &sender,
                            &event.content(),
                        )
                        .await
                    {
                        self.print_rendered_event(rendered);
                    }
                }
            }
            // TODO print out redacted messages.
            AnyRoomEvent::RedactedMessage(_) => (),
            AnyRoomEvent::RedactedState(_) => (),
            AnyRoomEvent::State(_) => (),
        }
    }

    pub fn room(&self) -> &Joined {
        &self.room
    }

    pub async fn handle_sync_state_event(
        &self,
        event: &AnySyncStateEvent,
        _state_event: bool,
    ) {
        match event {
            AnySyncStateEvent::RoomName(_) => self.update_buffer_name(),
            AnySyncStateEvent::RoomTopic(_) => self.set_topic(),
            AnySyncStateEvent::RoomCanonicalAlias(_) => self.set_alias(),
            _ => (),
        }
    }
}
