package main

import (
	"fmt"
	"os"

	"github.com/mark3labs/mcp-go/server"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
	"github.com/danweinerdev/lldb-debug-mcp/internal/tools"
)

func main() {
	s := server.NewMCPServer(
		"lldb-debug",
		"1.0.0",
		server.WithToolCapabilities(false),
	)

	sess := session.NewSessionManager()
	t := tools.New(sess)
	t.Register(s)

	if err := server.ServeStdio(s); err != nil {
		fmt.Fprintf(os.Stderr, "Server error: %v\n", err)
		os.Exit(1)
	}
}
