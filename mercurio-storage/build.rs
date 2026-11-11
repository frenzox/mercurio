//! Runs the PostgreSQL tests only when a database is given for them, and
//! lists them as ignored, with the variable to set, otherwise.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(postgres_tests)");
    println!("cargo::rerun-if-env-changed=MERCURIO_TEST_POSTGRES_URL");
    if std::env::var_os("MERCURIO_TEST_POSTGRES_URL").is_some() {
        println!("cargo::rustc-cfg=postgres_tests");
    }
}
