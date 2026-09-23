//! 文件/目录传输:清单、发送、接收、断点续传、SHA-256 校验。

pub mod manifest;
pub(crate) mod session;

pub use manifest::{ManifestSource, build_manifest, sanitize_entry_path};
pub use session::{CHUNK_SIZE, TransferDirection, is_transfer_kind};

pub(crate) use session::{TransferManager, route, start_send, start_send_update};
