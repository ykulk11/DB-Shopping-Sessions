# Shopping Sessions pipeline

A single statically linked Rust binary that implements `rules.md`: it cleans the event, click and
link CSVs under `/data`, joins linked user IDs into people, builds sessions, attributes clicks, and
writes `/output/user_daily/` and `/output/campaign_daily/` as Parquet.

- `src/main.rs`: the three phases (parse, dedup, sessionize) and orchestration
- `src/scan.rs`: SIMD scan for commas, newlines and quotes
- `src/ts.rs`: timestamp and number parsing
- `src/hash.rs`: ID hashing and small lookup tables
- `src/parquet.rs`: minimal Parquet writer
- `src/sys.rs`: file mapping and memory arenas

## Test

```sh
cargo build --release
python3 scripts/run_fixtures.py fixtures -- ./target/release/pipeline
./target/release/pipeline DATA_DIR OUTPUT_DIR && python3 scripts/compare.py OUTPUT_DIR dev-answer
```

With Docker: `docker buildx build --platform linux/amd64 --load -t shopping-sessions:latest .` then
`scripts/docker_run.sh DATA_DIR OUTPUT_DIR`.
