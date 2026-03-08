BINARY := lldb-debug-mcp
MODULE := github.com/danweinerdev/lldb-debug-mcp

.PHONY: build test test-integration clean

build:
	CGO_ENABLED=0 go build -o $(BINARY) ./cmd/$(BINARY)

test:
	go test -race ./...

test-integration: build
	$(MAKE) -C testdata
	go test -tags integration -race ./internal/tools/ -v

clean:
	rm -f $(BINARY)
	$(MAKE) -C testdata clean
