.PHONY: inspect verify arch contract test build

# The inspectable contract. No prose, just the system surface.
inspect:
	@cat contracts.inspect

arch:
	@./check-architecture.sh

contract:
	@./check-contract.sh

test:
	@cargo test --workspace

build:
	@cargo build --workspace

verify: arch contract test
	@echo "verify ok"
