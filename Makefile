BINARY := lldb-debug-mcp
MODULE := github.com/danweinerdev/lldb-debug-mcp
GOFLAGS := CGO_ENABLED=0

GOOS := $(shell go env GOOS)
GOARCH := $(shell go env GOARCH)
PLATFORM := $(GOOS)-$(GOARCH)

.PHONY: build build-all test test-integration clean

build:
	$(GOFLAGS) go build -o bin/$(PLATFORM)/$(BINARY) ./cmd/$(BINARY)

build-all:
	$(GOFLAGS) GOOS=linux   GOARCH=amd64 go build -o bin/linux-amd64/$(BINARY)   ./cmd/$(BINARY)
	$(GOFLAGS) GOOS=linux   GOARCH=arm64 go build -o bin/linux-arm64/$(BINARY)   ./cmd/$(BINARY)
	$(GOFLAGS) GOOS=darwin  GOARCH=amd64 go build -o bin/darwin-amd64/$(BINARY)  ./cmd/$(BINARY)
	$(GOFLAGS) GOOS=darwin  GOARCH=arm64 go build -o bin/darwin-arm64/$(BINARY)  ./cmd/$(BINARY)
	$(GOFLAGS) GOOS=windows GOARCH=amd64 go build -o bin/windows-amd64/$(BINARY).exe ./cmd/$(BINARY)
	$(GOFLAGS) GOOS=windows GOARCH=arm64 go build -o bin/windows-arm64/$(BINARY).exe ./cmd/$(BINARY)

test:
	go test -race ./...

test-integration: build
	$(MAKE) -C testdata
	go test -tags integration -race ./internal/tools/ -v

clean:
	rm -rf bin/
	$(MAKE) -C testdata clean
