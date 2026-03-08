BINARY := lldb-debug-mcp
MODULE := github.com/danweinerdev/lldb-debug-mcp
GOFLAGS := CGO_ENABLED=0

GOOS := $(shell go env GOOS)
GOARCH := $(shell go env GOARCH)
PLATFORM := $(GOOS)-$(GOARCH)

PLATFORMS := linux-amd64 linux-arm64 darwin-amd64 darwin-arm64 windows-amd64 windows-arm64

.PHONY: build build-all $(PLATFORMS) test test-integration clean

build:
	$(GOFLAGS) go build -o bin/$(PLATFORM)/$(BINARY) ./cmd/$(BINARY)

build-all: $(PLATFORMS)

linux-amd64:
	$(GOFLAGS) GOOS=linux GOARCH=amd64 go build -o bin/$@/$(BINARY) ./cmd/$(BINARY)

linux-arm64:
	$(GOFLAGS) GOOS=linux GOARCH=arm64 go build -o bin/$@/$(BINARY) ./cmd/$(BINARY)

darwin-amd64:
	$(GOFLAGS) GOOS=darwin GOARCH=amd64 go build -o bin/$@/$(BINARY) ./cmd/$(BINARY)

darwin-arm64:
	$(GOFLAGS) GOOS=darwin GOARCH=arm64 go build -o bin/$@/$(BINARY) ./cmd/$(BINARY)

windows-amd64:
	$(GOFLAGS) GOOS=windows GOARCH=amd64 go build -o bin/$@/$(BINARY).exe ./cmd/$(BINARY)

windows-arm64:
	$(GOFLAGS) GOOS=windows GOARCH=arm64 go build -o bin/$@/$(BINARY).exe ./cmd/$(BINARY)

test:
	go test -race ./...

test-integration: build
	$(MAKE) -C testdata
	go test -tags integration -race ./internal/tools/ -v

clean:
	rm -rf bin/
	$(MAKE) -C testdata clean
