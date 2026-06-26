pub mod new_wallet;
pub mod old_wallet;
pub mod payment_processor;
pub mod shared;
#[cfg(feature = "library_wallet")]
pub mod library_wallet;

// Shared gRPC types from wallet.proto (used by old_wallet and new_wallet)
#[allow(dead_code, clippy::doc_overindented_list_items)]
pub mod tari_rpc {
    tonic::include_proto!("tari.rpc");
}
