.PHONY: run play

run:
	cargo run --release -- $(ARGS)

play:
	cargo run --release -- --file "$(FILE)" $(ARGS)
