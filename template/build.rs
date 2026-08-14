//! `sqlx::migrate!` embeds `migrations/` into the binary at **compile**
//! time. Cargo does not otherwise watch that directory, so without the line
//! below a new migration file leaves the previous build cached — the
//! service boots reporting every migration applied while the new table
//! does not exist. Recorded here on day one rather than after the first
//! time it bites (`maxgame-news-backend/build.rs` has the same note).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
