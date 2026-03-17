mod engine;
mod llm;
mod lua;
mod tools;
mod utils;

fn main() {
    println!("ironcrew v{}", env!("CARGO_PKG_VERSION"));
}
