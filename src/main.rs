use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{Level, debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;

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
        let current_slot = self.get_current_slot().await?;
        debug!("Current slot: {}", current_slot);

        let write_cache = self.cached.read().await;
        let last_updated_index = self.last_updated_index.read().await;

        let last_written_slot = if *last_updated_index == 0 {
            write_cache[BUF_SIZE - 1]
        } else {
            write_cache[*last_updated_index - 1]
        };

        debug!("Last cached index: {}", last_updated_index);

        let start_slot = if last_written_slot == 0 {
            current_slot
        } else {
            last_written_slot.min(current_slot)
        };

        debug!("Last cached block: {}", last_written_slot);

        let blocks = self
            .get_confirmed_blocks_between_slots(start_slot, Some(current_slot))
            .await?;

        drop(last_updated_index);
        drop(write_cache);

        self.update_cache(blocks).await;

        Ok(())
    }

    async fn update_cache(&self, blocks: Vec<u64>) {
        let len = blocks.len();

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

        if len > 0 {
            info!("{} cached blocks updated", len);
        }

        drop(last_updated_index);
        drop(write_cache);
    }
}

async fn bg_cache_syncer(state: Arc<AppState>) {
    loop {
        if let Err(e) = state.cache_sync_latest().await {
            error!("{}", e);
        };
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
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
                let message = format!("Failed to get blocks: {:?}", e);
                error!("{}", message);
                (StatusCode::NOT_ACCEPTABLE, message)
            })?;

        if blocks.len() > 0 {
            // TODO Should cache, but since this block could be far back enough that
            //      would cause the next sync to fail due to too many blocks
            //      found, skip caching and return the u64 as a confirmed block.
            //
            //      state.update_cache(blocks).await;
            //
            Ok((StatusCode::OK, slot.to_string()))
        } else {
            Err((StatusCode::NOT_FOUND, String::new()))
        }
    } else {
        Ok((StatusCode::OK, slot.to_string()))
    }
}

fn init() {
    let log_level = std::env::var("LOG_LEVEL")
        .unwrap_or(String::from("warn"))
        .to_lowercase();

    if !["none"].contains(&log_level.as_str()) || !log_level.is_empty() {
        let level = if ["-1", "error"].contains(&log_level.as_str()) {
            Level::ERROR
        } else if ["0", "warn", "warning"].contains(&log_level.as_str()) {
            Level::WARN
        } else if ["1", "info", "default"].contains(&log_level.as_str()) {
            Level::INFO
        } else if ["2", "debug"].contains(&log_level.as_str()) {
            Level::DEBUG
        } else if ["3", "trace", "tracing"].contains(&log_level.as_str()) {
            Level::TRACE
        } else {
            Level::INFO
        };

        let subscriber = FmtSubscriber::builder().with_max_level(level).finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("setting default subscriber failed");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init();

    // TODO find out why my api key is not working
    let syndica_api_url = if let Ok(s) = std::env::var("SYNDICA_API_URL") {
        s
    } else {
        warn!("SYNDICA_API_URL was not provided. Using default public API");
        String::from("https://api.mainnet-beta.solana.com")
    };

    let state = Arc::new(AppState {
        client: RpcClient::new(syndica_api_url),
        cached: RwLock::new([0; BUF_SIZE]),
        last_updated_index: RwLock::new(0),
    });

    let cache_state = Arc::clone(&state);
    tokio::task::spawn(async move { bg_cache_syncer(cache_state).await });

    let app = Router::new()
        .route("/isSlotConfirmed/{slot}", get(get_is_slot_confirmed))
        .with_state(state);

    let bind_url = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(bind_url).await.unwrap(); // Exit if app can't use the bind address
    info!("Starting server on {bind_url}");
    axum::serve(listener, app).await.unwrap(); // Exit if app fails to start

    Ok(())
}
