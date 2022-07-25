build:
  cargo build

test:
  cargo nextest run

# test without proptest
test-fast:
  RUSTFLAGS='--cfg no_proptest' cargo nextest run

check-unsafe:
  env RUSTFLAGS="-Funsafe-code --cap-lints=warn" cargo check

deny:
  cargo deny check

crev:
  cargo crev crate check
