#[macro_use]
extern crate log;

use log::LevelFilter;
use simple_logger::SimpleLogger;
use smol::io;

mod app;
use app::App;

fn main() -> io::Result<()> {
    SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .init()
        .expect("A logger was already initialized");

    info!("Basic Naia Server Demo started");

    smol::block_on(async {
        let mut app = App::new().await;
        loop {
            app.update().await;
        }
    })
}
