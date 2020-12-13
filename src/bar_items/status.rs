use weechat::{
    buffer::Buffer,
    hooks::{BarItem, BarItemCallback},
    Weechat,
};

use crate::{BufferOwner, Servers};

pub(super) struct Status {
    servers: Servers,
}

impl Status {
    pub(super) fn create(servers: Servers) -> Result<BarItem, ()> {
        let status = Status { servers };
        BarItem::new("buffer_modes", status)
    }
}

impl BarItemCallback for Status {
    fn callback(&mut self, _: &Weechat, buffer: &Buffer) -> String {
        let mut signs = Vec::new();

        if let BufferOwner::Room(server, room) =
            self.servers.buffer_owner(buffer)
        {
            if room.is_encrypted() {
                signs.push(
                    server.config().borrow().look().encrypted_room_sign(),
                );
            }

            if room.is_busy() {
                signs.push("⏳".to_owned());
            }
        }

        signs.join("")
    }
}
