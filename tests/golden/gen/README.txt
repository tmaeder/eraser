Go programs that produced the golden files. Go's internal/ rule requires them to live inside the
module, so to regenerate: copy go.mod, go.sum and internal/ to a temp dir, put each file at
<tmp>/cmd/<name>/main.go, then run from <tmp>:
  go run ./cmd/golden   <repo>/tests/golden <repo>/data/brokers.yaml
  go run ./cmd/goldendb <repo>/tests/golden/history_go.sqlite <repo>/tests/golden/history_go.json
  go run ./cmd/readrs   <db written by: ERASER_RUST_DB=<db> cargo test write_db_for_go -- --ignored>
