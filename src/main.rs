use axum::extract::Path;
use axum::extract::State;
use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::msg;
use std::sync::Arc;
use tokio::sync::RwLock;

const BUF_SIZE: usize = 10;

struct AppState {
    client: RpcClient,
    cached: RwLock<[u64; BUF_SIZE]>,
    last_updated_index: RwLock<usize>,
}

impl AppState {
    async fn get_current_slot(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        self.client.get_slot().await.map_err(|e| e.into())
    }

    async fn get_confirmed_blocks_between_slots(
        &self,
        start_slot: u64,
        end_slot: Option<u64>,
    ) -> Result<Vec<u64>, Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .get_blocks(start_slot, end_slot)
            .await
            .map_err(|e| e.into())
    }

    async fn cache_sync_latest(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let last_updated_index = self.last_updated_index.read().await;
        msg!("Last updated index: {:?}", last_updated_index);
        drop(last_updated_index);

        // client.get_slot_with_commitment(CommitmentConfig { commitment: CommitmentLevel::Confirmed })
        let current_slot = self.get_current_slot().await?;
        msg!("Current slot: {}", current_slot);

        let write_cache = self.cached.read().await;
        let last_updated_index = self.last_updated_index.read().await;

        let last_written_slot = if *last_updated_index == 0 {
            write_cache[BUF_SIZE - 1]
        } else {
            write_cache[*last_updated_index - 1]
        };

        msg!("Current index: {}", last_updated_index);

        let start_slot = if last_written_slot == 0 {
            current_slot
        } else {
            last_written_slot.min(current_slot)
        };

        msg!("Last written slot: {}", last_written_slot);

        let blocks = self
            .get_confirmed_blocks_between_slots(start_slot, Some(current_slot))
            .await?;

        drop(last_updated_index);
        drop(write_cache);

        self.update_cache(blocks).await;

        Ok(())
    }

    async fn update_cache(&self, blocks: Vec<u64>) {
        let mut write_cache = self.cached.write().await;
        let mut last_updated_index = self.last_updated_index.write().await;

        for block in blocks {
            write_cache[*last_updated_index] = block;
            if *last_updated_index == BUF_SIZE - 1 {
                *last_updated_index = 0
            } else {
                *last_updated_index += 1;
            }
        }

        println!("{:?}", *write_cache);
        drop(last_updated_index);
        drop(write_cache);
    }
}

async fn bg_cache_syncer(
    state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        state.cache_sync_latest().await?;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // TODO find out why my api key is not working
    let syndica_api_url = std::env::var("SYNDICA_API_URL")
        .unwrap_or(String::from("https://api.mainnet-beta.solana.com"));

    // tracing_subscriber::fmt::init();

    let client = RpcClient::new(syndica_api_url);

    let state = Arc::new(AppState {
        client: client,
        cached: RwLock::new([0; BUF_SIZE]),
        last_updated_index: RwLock::new(0),
    });

    let cache_state = Arc::clone(&state);
    tokio::task::spawn(async move { bg_cache_syncer(cache_state).await });

    let app = Router::new()
        .route("/isSlotConfirmed/{slot}", get(get_is_slot_confirmed))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}

async fn get_is_slot_confirmed(
    Path(slot): Path<u64>,
    State(state): State<Arc<AppState>>,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    let state = Arc::clone(&state);

    // Get cache to check
    let ro_cached = state.cached.read().await;
    let cached = ro_cached.clone();
    drop(ro_cached);
    if !cached.contains(&slot) {
        // Not found, update cache manually and check again
        let state: Arc<AppState> = Arc::clone(&state);
        let blocks = state
            .get_confirmed_blocks_between_slots(slot, Some(slot))
            .await
            .map_err(|e| {
                (
                    StatusCode::NOT_ACCEPTABLE,
                    format!("Failed to get blocks: {:?}", e),
                )
            })?;

        if blocks.len() > 0 {
            state.update_cache(blocks).await;
            Ok((StatusCode::OK, slot.to_string()))
        } else {
            Err((StatusCode::NOT_FOUND, String::new()))
        }
    } else {
        Ok((StatusCode::OK, slot.to_string()))
    }
}
