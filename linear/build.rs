fn main() {
    println!("cargo:rerun-if-changed=../app/migrations");
}
