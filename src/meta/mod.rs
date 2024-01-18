use std::time::Instant;

mod config;
pub use config::MetaConfig;
pub mod engine;
mod err;
pub use err::MetaError;
mod engine_quota;
mod engine_sto;
pub mod types;
mod util;

pub mod internal_nodes {
    use std::{collections::HashMap, time::Duration};

    use crate::meta::{
        types::{
            Entry, Ino, InodeAttr, CONFIG_INODE, CONTROL_INODE, LOG_INODE, MAX_INTERNAL_INODE,
            STATS_INODE,
        },
        util::UID_GID,
    };

    pub const LOG_INODE_NAME: &'static str = ".accesslog";
    pub const CONTROL_INODE_NAME: &'static str = ".control";
    pub const STATS_INODE_NAME: &'static str = ".stats";
    pub const CONFIG_INODE_NAME: &'static str = ".config";
    pub const TRASH_INODE_NAME: &'static str = ".trash";
    #[derive(Debug)]
    pub struct PreInternalNodes {
        nodes: HashMap<&'static str, InternalNode>,
    }

    impl PreInternalNodes {
        pub fn new(entry_timeout: (Duration, Duration)) -> Self {
            let mut map = HashMap::new();
            let control_inode: InternalNode = InternalNode(Entry {
                inode: CONTROL_INODE,
                name: CONTROL_INODE_NAME.to_string(),
                attr: InodeAttr::default().set_perm(0o666).set_full().to_owned(),
                ttl: Some(entry_timeout.0),
                generation: Some(1),
            });
            let log_inode: InternalNode = InternalNode(Entry {
                inode: LOG_INODE,
                name: LOG_INODE_NAME.to_string(),
                attr: InodeAttr::default().set_perm(0o400).set_full().to_owned(),
                ttl: Some(entry_timeout.0),
                generation: Some(1),
            });
            let stats_inode: InternalNode = InternalNode(Entry {
                inode: STATS_INODE,
                name: STATS_INODE_NAME.to_string(),
                attr: InodeAttr::default().set_perm(0o400).set_full().to_owned(),
                ttl: Some(entry_timeout.0),
                generation: Some(1),
            });
            let config_inode: InternalNode = InternalNode(Entry {
                inode: CONFIG_INODE,
                name: CONFIG_INODE_NAME.to_string(),
                attr: InodeAttr::default().set_perm(0o400).set_full().to_owned(),
                ttl: Some(entry_timeout.0),
                generation: Some(1),
            });
            let trash_inode: InternalNode = InternalNode(Entry {
                inode: MAX_INTERNAL_INODE,
                name: TRASH_INODE_NAME.to_string(),
                attr: InodeAttr::default()
                    .set_perm(0o555)
                    .set_kind(fuser::FileType::Directory)
                    .set_nlink(2)
                    .set_uid(UID_GID.0)
                    .set_gid(UID_GID.1)
                    .set_full()
                    .to_owned(),
                ttl: Some(entry_timeout.1),
                generation: Some(1),
            });
            map.insert(LOG_INODE_NAME, log_inode);
            map.insert(CONTROL_INODE_NAME, control_inode);
            map.insert(STATS_INODE_NAME, stats_inode);
            map.insert(CONFIG_INODE_NAME, config_inode);
            map.insert(TRASH_INODE_NAME, trash_inode);
            Self { nodes: map }
        }
    }

    impl PreInternalNodes {
        pub fn get_internal_node_by_name(&self, name: &str) -> Option<&InternalNode> {
            self.nodes.get(name)
        }
        pub fn get_internal_node(&self, ino: Ino) -> Option<&InternalNode> {
            self.nodes.values().find(|node| node.0.inode == ino)
        }
        pub fn get_mut_internal_node_by_name(&mut self, name: &str) -> Option<&mut InternalNode> {
            self.nodes.get_mut(name)
        }
    }

    #[derive(Debug)]
    pub struct InternalNode(Entry);

    impl Into<Entry> for InternalNode {
        fn into(self) -> Entry {
            self.0
        }
    }

    impl Into<Entry> for &'_ InternalNode {
        fn into(self) -> Entry {
            self.0.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaContext {
    pub gid: u32,
    pub gid_list: Vec<u32>,
    pub uid: u32,
    pub pid: u32,
    pub check_permission: bool,
    pub start_at: Instant,
}

impl Default for MetaContext {
    fn default() -> Self {
        Self {
            gid: 0,
            gid_list: vec![],
            uid: 0,
            pid: 0,
            check_permission: false,
            start_at: Instant::now(),
        }
    }
}

pub const MAX_NAME_LENGTH: usize = 255;
pub const DOT: &'static str = ".";
pub const DOT_DOT: &'static str = "..";

pub const MODE_MASK_R: u8 = 0b100;
pub const MODE_MASK_W: u8 = 0b010;
pub const MODE_MASK_X: u8 = 0b001;
