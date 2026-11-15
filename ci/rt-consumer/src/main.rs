//! The attribute form of `main`, in a crate with no tokio dependency.

#[ferroly::rt::main]
async fn main() {
    let value = ferroly::rt::spawn(async { 40 + 2 }).await.unwrap();
    assert_eq!(value, 42);
    println!("rt::main ok: {value}");
}
