use runkv_common::config::{CacheConfig, LsmTreeConfig, MinioConfig, Node, S3Config};
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct WheelConfig {
    pub id: u64,
    pub host: String,
    pub port: u16,
    pub data_path: String,
    pub meta_path: String,
    pub poll_interval: String,
    pub heartbeat_interval: String,
    pub rudder: Node,
    pub s3: Option<S3Config>,
    pub minio: Option<MinioConfig>,
    pub buffer: BufferConfig,
    pub cache: CacheConfig,
    pub lsm_tree: LsmTreeConfig,
    pub raft_log_store: RaftLogStoreConfig,
}

#[derive(Deserialize, Clone, Debug)]
pub struct BufferConfig {
    pub write_buffer_capacity: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RaftLogStoreConfig {
    pub log_dir_path: String,
    pub log_file_capacity: String,
    pub block_cache_capacity: String,
}
