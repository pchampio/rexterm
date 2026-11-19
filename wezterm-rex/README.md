# WezTerm Rex protocol groundwork

This crate is not yet connected to WezTerm panes. See `../REX-MIGRATION.txt`
for implementation status, remaining parity work and the snapshot fidelity
constraint. It does not replace the tmux launcher.

Build with Rust 1.97+ and Zig 0.17.x (`ZIG=/path/to/zig`). Dependencies are pinned
in `Cargo.lock`. `cargo test --locked` runs the offline parser checks; the live
test is explicitly ignored unless requested, and requires `REX_TEST_ENDPOINT`
pointing to a disposable Rex server. It creates and destroys only its own test
session and never starts or reconfigures a default server.

`transport::Client::call_reply` preserves the response sequence needed to order
bootstrap views against buffered events. Requests have a timeout; a timeout
means the operation's outcome is unknown and must not be automatically replayed.
Control reconnect and a native pane adapter remain unimplemented. Only Unix
transport has been tested live; HTTPS/TLS support has not been exercised.
