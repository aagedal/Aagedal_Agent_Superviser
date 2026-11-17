//! Prints the link the app opens to install Apple's `container`.

#[tokio::main]
async fn main() {
    println!("{}", superviser_core::sandbox::container::installer_url().await);
}
