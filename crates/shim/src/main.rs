use std::sync::Arc;

use limen_shim::config::Config;
use limen_shim::gadget::UnixGadget;
use limen_shim::http::Shim;
use limen_shim::replies::{FsReplyWait, prepare_replies_dir};
use limen_shim::server::{serve, startup};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("limen-shim: {e}");
            std::process::exit(1);
        }
    };
    let gadget = UnixGadget::new(&config);
    if let Err(e) = startup(&mut config, &gadget).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
    config.reply_wait_s = prepare_replies_dir(&config.replies_dir, config.reply_wait_s);
    let replies = FsReplyWait::new(config.replies_dir.clone(), config.reply_poll);
    let shim = Shim::new(config, Arc::new(gadget), Arc::new(replies));
    if let Err(e) = serve(shim).await {
        eprintln!("limen-shim: {e}");
        std::process::exit(1);
    }
}
