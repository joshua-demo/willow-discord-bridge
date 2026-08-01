#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    willow_discord_bridge_lib::run();
}
