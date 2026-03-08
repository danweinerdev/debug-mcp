package dap

import (
	"bufio"
	"context"
	"errors"
	"io"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	godap "github.com/google/go-dap"
)

// makeInitializeResponse creates an InitializeResponse with the given seq and
// requestSeq values, ready for writing to a pipe.
func makeInitializeResponse(seq, requestSeq int) *godap.InitializeResponse {
	resp := &godap.InitializeResponse{}
	resp.Seq = seq
	resp.Type = "response"
	resp.Command = "initialize"
	resp.RequestSeq = requestSeq
	resp.Success = true
	return resp
}

func TestSendReceive(t *testing.T) {
	// Create two pipe pairs: one for client writes (requests), one for client reads (responses).
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)
	go client.ReadLoop()

	// In a goroutine, read the request from the other end of the client's
	// write pipe, then write a matching response to the client's read pipe.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		msg, err := godap.ReadProtocolMessage(reader)
		if err != nil {
			t.Errorf("failed to read request from pipe: %v", err)
			return
		}

		req, ok := msg.(godap.RequestMessage)
		if !ok {
			t.Errorf("expected RequestMessage, got %T", msg)
			return
		}

		resp := makeInitializeResponse(1, req.GetRequest().Seq)
		if err := godap.WriteProtocolMessage(clientReadPW, resp); err != nil {
			t.Errorf("failed to write response: %v", err)
		}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	initReq := &godap.InitializeRequest{}
	initReq.Type = "request"
	initReq.Command = "initialize"
	initReq.Arguments = godap.InitializeRequestArguments{
		ClientID:  "test",
		AdapterID: "lldb-dap",
	}

	result, err := client.Send(ctx, initReq)
	if err != nil {
		t.Fatalf("Send returned error: %v", err)
	}

	resp, ok := result.(*godap.InitializeResponse)
	if !ok {
		t.Fatalf("expected *InitializeResponse, got %T", result)
	}
	if !resp.Success {
		t.Error("expected Success to be true")
	}
	if resp.Command != "initialize" {
		t.Errorf("Command: got %q, want %q", resp.Command, "initialize")
	}
}

func TestSendMultipleConcurrent(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)
	go client.ReadLoop()

	const numRequests = 3

	// Goroutine that reads all requests, collects their sequence numbers,
	// then responds in reverse order.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		seqs := make([]int, 0, numRequests)

		for i := 0; i < numRequests; i++ {
			msg, err := godap.ReadProtocolMessage(reader)
			if err != nil {
				t.Errorf("failed to read request %d: %v", i, err)
				return
			}
			req, ok := msg.(godap.RequestMessage)
			if !ok {
				t.Errorf("expected RequestMessage, got %T", msg)
				return
			}
			seqs = append(seqs, req.GetRequest().Seq)
		}

		// Respond in reverse order.
		for i := len(seqs) - 1; i >= 0; i-- {
			resp := makeInitializeResponse(100+i, seqs[i])
			if err := godap.WriteProtocolMessage(clientReadPW, resp); err != nil {
				t.Errorf("failed to write response for seq %d: %v", seqs[i], err)
				return
			}
		}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	var wg sync.WaitGroup
	results := make([]godap.Message, numRequests)
	errs := make([]error, numRequests)

	for i := 0; i < numRequests; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			req := &godap.InitializeRequest{}
			req.Type = "request"
			req.Command = "initialize"
			req.Arguments = godap.InitializeRequestArguments{
				ClientID:  "test",
				AdapterID: "lldb-dap",
			}
			results[idx], errs[idx] = client.Send(ctx, req)
		}(i)
	}

	wg.Wait()

	for i := 0; i < numRequests; i++ {
		if errs[i] != nil {
			t.Errorf("Send[%d] returned error: %v", i, errs[i])
			continue
		}
		resp, ok := results[i].(*godap.InitializeResponse)
		if !ok {
			t.Errorf("Send[%d]: expected *InitializeResponse, got %T", i, results[i])
			continue
		}
		if !resp.Success {
			t.Errorf("Send[%d]: expected Success=true", i)
		}
	}
}

func TestSendAsync(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)
	go client.ReadLoop()

	// Read the request and respond.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		msg, err := godap.ReadProtocolMessage(reader)
		if err != nil {
			t.Errorf("failed to read request: %v", err)
			return
		}
		req, ok := msg.(godap.RequestMessage)
		if !ok {
			t.Errorf("expected RequestMessage, got %T", msg)
			return
		}
		resp := makeInitializeResponse(1, req.GetRequest().Seq)
		if err := godap.WriteProtocolMessage(clientReadPW, resp); err != nil {
			t.Errorf("failed to write response: %v", err)
		}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req := &godap.InitializeRequest{}
	req.Type = "request"
	req.Command = "initialize"
	req.Arguments = godap.InitializeRequestArguments{
		ClientID:  "test",
		AdapterID: "lldb-dap",
	}

	ch, err := client.SendAsync(ctx, req)
	if err != nil {
		t.Fatalf("SendAsync returned error: %v", err)
	}

	select {
	case result := <-ch:
		if result.err != nil {
			t.Fatalf("result.err: %v", result.err)
		}
		resp, ok := result.msg.(*godap.InitializeResponse)
		if !ok {
			t.Fatalf("expected *InitializeResponse, got %T", result.msg)
		}
		if !resp.Success {
			t.Error("expected Success=true")
		}
	case <-ctx.Done():
		t.Fatal("timed out waiting for async response")
	}
}

func TestSendContextCancellation(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	// Use a writer that we never read from, so the write blocks are handled,
	// but we use a discard-style approach: we read and discard.
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)
	go client.ReadLoop()

	// Drain the write side so Send doesn't block on write.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		for {
			_, err := godap.ReadProtocolMessage(reader)
			if err != nil {
				return
			}
		}
	}()

	ctx, cancel := context.WithCancel(context.Background())

	req := &godap.InitializeRequest{}
	req.Type = "request"
	req.Command = "initialize"
	req.Arguments = godap.InitializeRequestArguments{
		ClientID:  "test",
		AdapterID: "lldb-dap",
	}

	// Use SendAsync so we can cancel between sending and receiving.
	ch, err := client.SendAsync(ctx, req)
	if err != nil {
		t.Fatalf("SendAsync returned error: %v", err)
	}

	// Cancel the context before any response arrives.
	cancel()

	// Now use a select simulating what Send would do.
	select {
	case result := <-ch:
		// The response channel might also receive if cancelAllPending
		// ran, but in this case we never close the reader, so the read
		// loop is still running. The channel should not receive.
		t.Fatalf("unexpected result on channel: %+v", result)
	case <-ctx.Done():
		// Expected: context was cancelled.
	}

	if ctx.Err() != context.Canceled {
		t.Errorf("expected context.Canceled, got %v", ctx.Err())
	}

	// Verify the pending entry is cleaned up. We access it through Send
	// which cleans up on cancellation.
	seq := req.GetRequest().Seq
	client.pendingMu.Lock()
	_, found := client.pending[seq]
	client.pendingMu.Unlock()

	// The pending entry should still exist since we used SendAsync
	// (Send does the cleanup, not SendAsync). Clean it up manually.
	if found {
		client.pendingMu.Lock()
		delete(client.pending, seq)
		client.pendingMu.Unlock()
	}
}

func TestSendContextCancellationViaSend(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)
	go client.ReadLoop()

	// Drain the write side so Send doesn't block on write.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		for {
			_, err := godap.ReadProtocolMessage(reader)
			if err != nil {
				return
			}
		}
	}()

	ctx, cancel := context.WithCancel(context.Background())

	req := &godap.InitializeRequest{}
	req.Type = "request"
	req.Command = "initialize"
	req.Arguments = godap.InitializeRequestArguments{
		ClientID:  "test",
		AdapterID: "lldb-dap",
	}

	// Cancel immediately so Send returns quickly.
	cancel()

	result, err := client.Send(ctx, req)
	if err == nil {
		t.Fatalf("expected error from Send, got result: %v", result)
	}
	if !errors.Is(err, context.Canceled) {
		t.Errorf("expected context.Canceled, got %v", err)
	}

	// Verify the pending entry was cleaned up.
	seq := req.GetRequest().Seq
	client.pendingMu.Lock()
	_, found := client.pending[seq]
	client.pendingMu.Unlock()
	if found {
		t.Error("expected pending entry to be cleaned up after context cancellation")
	}
}

func TestCancelAllPending(t *testing.T) {
	// We don't need real pipes here; just test the pending map management.
	// Create a client with a dummy reader/writer that we won't use.
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)

	// Drain the write side so SendAsync doesn't block.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		for {
			_, err := godap.ReadProtocolMessage(reader)
			if err != nil {
				return
			}
		}
	}()

	ctx := context.Background()
	const numRequests = 3
	channels := make([]<-chan pendingResult, numRequests)

	for i := 0; i < numRequests; i++ {
		req := &godap.InitializeRequest{}
		req.Type = "request"
		req.Command = "initialize"
		req.Arguments = godap.InitializeRequestArguments{
			ClientID:  "test",
			AdapterID: "lldb-dap",
		}
		ch, err := client.SendAsync(ctx, req)
		if err != nil {
			t.Fatalf("SendAsync[%d] failed: %v", i, err)
		}
		channels[i] = ch
	}

	// Verify all 3 entries are in the pending map.
	client.pendingMu.Lock()
	if len(client.pending) != numRequests {
		t.Fatalf("expected %d pending entries, got %d", numRequests, len(client.pending))
	}
	client.pendingMu.Unlock()

	// Cancel all pending.
	cancelErr := errors.New("connection closed")
	client.cancelAllPending(cancelErr)

	// Verify the pending map is empty.
	client.pendingMu.Lock()
	if len(client.pending) != 0 {
		t.Errorf("expected 0 pending entries after cancelAllPending, got %d", len(client.pending))
	}
	client.pendingMu.Unlock()

	// Verify each channel received an error.
	for i, ch := range channels {
		select {
		case result := <-ch:
			if result.err == nil {
				t.Errorf("channel[%d]: expected error, got nil", i)
			} else if !errors.Is(result.err, cancelErr) {
				t.Errorf("channel[%d]: expected %v, got %v", i, cancelErr, result.err)
			}
			if result.msg != nil {
				t.Errorf("channel[%d]: expected nil msg, got %T", i, result.msg)
			}
		default:
			t.Errorf("channel[%d]: expected result to be available immediately", i)
		}
	}
}

func TestNextSeqIncrementing(t *testing.T) {
	client := NewClient(bufio.NewReader(&io.LimitedReader{R: nil, N: 0}), io.Discard)

	for i := 1; i <= 10; i++ {
		got := client.nextSeq()
		if got != i {
			t.Errorf("nextSeq() call %d: got %d, want %d", i, got, i)
		}
	}
}

func TestDispatchResponseNoWaiter(t *testing.T) {
	// dispatchResponse should not panic when there is no waiter.
	client := NewClient(bufio.NewReader(&io.LimitedReader{R: nil, N: 0}), io.Discard)

	resp := makeInitializeResponse(1, 999)
	// Should log but not panic.
	client.dispatchResponse(resp)
}

func TestReadLoopInterleavedResponsesAndEvents(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)

	outputCh := make(chan *godap.OutputEvent, 1)
	client.SetOutputHandler(func(event *godap.OutputEvent) {
		outputCh <- event
	})

	go client.ReadLoop()

	// Drain the write side so SendAsync doesn't block.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		for {
			_, err := godap.ReadProtocolMessage(reader)
			if err != nil {
				return
			}
		}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Send a request to register a pending entry.
	req := &godap.InitializeRequest{}
	req.Type = "request"
	req.Command = "initialize"
	req.Arguments = godap.InitializeRequestArguments{
		ClientID:  "test",
		AdapterID: "lldb-dap",
	}

	ch, err := client.SendAsync(ctx, req)
	if err != nil {
		t.Fatalf("SendAsync returned error: %v", err)
	}

	seqNum := req.GetRequest().Seq

	// Write an InitializedEvent first.
	initEvent := &godap.InitializedEvent{}
	initEvent.Type = "event"
	initEvent.Event.Event = "initialized"
	if err := godap.WriteProtocolMessage(clientReadPW, initEvent); err != nil {
		t.Fatalf("failed to write InitializedEvent: %v", err)
	}

	// Write an OutputEvent.
	outEvent := &godap.OutputEvent{}
	outEvent.Type = "event"
	outEvent.Event.Event = "output"
	outEvent.Body = godap.OutputEventBody{
		Category: "stdout",
		Output:   "hello world\n",
	}
	if err := godap.WriteProtocolMessage(clientReadPW, outEvent); err != nil {
		t.Fatalf("failed to write OutputEvent: %v", err)
	}

	// Write the response for the pending request.
	resp := makeInitializeResponse(10, seqNum)
	if err := godap.WriteProtocolMessage(clientReadPW, resp); err != nil {
		t.Fatalf("failed to write InitializeResponse: %v", err)
	}

	// Verify initialized event.
	select {
	case <-client.InitializedChan():
		// Expected.
	case <-ctx.Done():
		t.Fatal("timed out waiting for InitializedEvent")
	}

	// Verify response.
	select {
	case result := <-ch:
		if result.err != nil {
			t.Fatalf("expected nil error, got: %v", result.err)
		}
		if _, ok := result.msg.(*godap.InitializeResponse); !ok {
			t.Fatalf("expected *InitializeResponse, got %T", result.msg)
		}
	case <-ctx.Done():
		t.Fatal("timed out waiting for response")
	}

	// Verify output event.
	select {
	case event := <-outputCh:
		if event.Body.Output != "hello world\n" {
			t.Errorf("Output: got %q, want %q", event.Body.Output, "hello world\n")
		}
		if event.Body.Category != "stdout" {
			t.Errorf("Category: got %q, want %q", event.Body.Category, "stdout")
		}
	case <-ctx.Done():
		t.Fatal("timed out waiting for OutputEvent callback")
	}
}

func TestReadLoopStoppedEvent(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), io.Discard)

	var stoppedCallback atomic.Value
	client.SetOnStopped(func(event *godap.StoppedEvent) {
		stoppedCallback.Store(event)
	})

	go client.ReadLoop()

	// Register a waiter before the event arrives.
	waiterCh := client.StopWaiter().Register()

	// Write a StoppedEvent.
	event := &godap.StoppedEvent{}
	event.Type = "event"
	event.Event.Event = "stopped"
	event.Body = godap.StoppedEventBody{
		Reason:   "breakpoint",
		ThreadId: 1,
	}
	if err := godap.WriteProtocolMessage(clientReadPW, event); err != nil {
		t.Fatalf("failed to write StoppedEvent: %v", err)
	}

	// Verify StopWaiter receives the event.
	select {
	case result := <-waiterCh:
		if result.Event == nil {
			t.Fatal("expected Event to be non-nil")
		}
		if result.Event.Body.Reason != "breakpoint" {
			t.Errorf("Reason: got %q, want %q", result.Event.Body.Reason, "breakpoint")
		}
		if result.Event.Body.ThreadId != 1 {
			t.Errorf("ThreadId: got %d, want 1", result.Event.Body.ThreadId)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for StopResult")
	}

	// Verify onStopped callback was called.
	v := stoppedCallback.Load()
	if v == nil {
		t.Fatal("expected onStopped callback to be called")
	}
	cbEvent := v.(*godap.StoppedEvent)
	if cbEvent.Body.Reason != "breakpoint" {
		t.Errorf("callback Reason: got %q, want %q", cbEvent.Body.Reason, "breakpoint")
	}
}

func TestReadLoopExitedEvent(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), io.Discard)

	var exitCode atomic.Int32
	exitCode.Store(-1) // sentinel value
	client.SetOnExit(func(code int) {
		exitCode.Store(int32(code))
	})

	go client.ReadLoop()

	// Register a waiter.
	waiterCh := client.StopWaiter().Register()

	// Write an ExitedEvent.
	event := &godap.ExitedEvent{}
	event.Type = "event"
	event.Event.Event = "exited"
	event.Body = godap.ExitedEventBody{
		ExitCode: 42,
	}
	if err := godap.WriteProtocolMessage(clientReadPW, event); err != nil {
		t.Fatalf("failed to write ExitedEvent: %v", err)
	}

	// Verify StopWaiter receives the exit.
	select {
	case result := <-waiterCh:
		if !result.Exited {
			t.Error("expected Exited to be true")
		}
		if result.ExitCode == nil {
			t.Fatal("expected ExitCode to be non-nil")
		}
		if *result.ExitCode != 42 {
			t.Errorf("ExitCode: got %d, want 42", *result.ExitCode)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for StopResult")
	}

	// Verify onExit callback was called.
	if got := exitCode.Load(); got != 42 {
		t.Errorf("onExit exitCode: got %d, want 42", got)
	}
}

func TestReadLoopTerminatedEvent(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), io.Discard)

	var terminated atomic.Bool
	client.SetOnTerminated(func() {
		terminated.Store(true)
	})

	go client.ReadLoop()

	// Register a waiter.
	waiterCh := client.StopWaiter().Register()

	// Write a TerminatedEvent.
	event := &godap.TerminatedEvent{}
	event.Type = "event"
	event.Event.Event = "terminated"
	if err := godap.WriteProtocolMessage(clientReadPW, event); err != nil {
		t.Fatalf("failed to write TerminatedEvent: %v", err)
	}

	// Verify StopWaiter receives the cancel.
	select {
	case result := <-waiterCh:
		if !result.Terminated {
			t.Error("expected Terminated to be true")
		}
		if result.Event != nil {
			t.Error("expected Event to be nil")
		}
		if result.Exited {
			t.Error("expected Exited to be false")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for StopResult")
	}

	// Verify onTerminated callback was called.
	if !terminated.Load() {
		t.Error("expected onTerminated callback to be called")
	}
}

func TestReadLoopEOF(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), clientWritePW)

	// Track whether onTerminated is called on EOF (crash recovery).
	var terminated atomic.Bool
	client.SetOnTerminated(func() {
		terminated.Store(true)
	})

	// Drain the write side so SendAsync doesn't block.
	go func() {
		reader := bufio.NewReader(clientWritePR)
		for {
			_, err := godap.ReadProtocolMessage(reader)
			if err != nil {
				return
			}
		}
	}()

	go client.ReadLoop()

	// Register a StopWaiter.
	waiterCh := client.StopWaiter().Register()

	// Create pending requests.
	ctx := context.Background()
	const numRequests = 2
	channels := make([]<-chan pendingResult, numRequests)
	for i := 0; i < numRequests; i++ {
		req := &godap.InitializeRequest{}
		req.Type = "request"
		req.Command = "initialize"
		req.Arguments = godap.InitializeRequestArguments{
			ClientID:  "test",
			AdapterID: "lldb-dap",
		}
		ch, err := client.SendAsync(ctx, req)
		if err != nil {
			t.Fatalf("SendAsync[%d] failed: %v", i, err)
		}
		channels[i] = ch
	}

	// Close the reader to trigger EOF.
	clientReadPW.Close()

	// Verify all pending requests receive errors.
	for i, ch := range channels {
		select {
		case result := <-ch:
			if result.err == nil {
				t.Errorf("channel[%d]: expected error, got nil", i)
			}
		case <-time.After(5 * time.Second):
			t.Fatalf("channel[%d]: timed out waiting for error", i)
		}
	}

	// Verify StopWaiter was cancelled.
	select {
	case result := <-waiterCh:
		if !result.Terminated {
			t.Error("expected StopWaiter result to have Terminated=true")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for StopWaiter cancel")
	}

	// Verify the client is closed.
	select {
	case <-client.Closed():
		// Expected.
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for client to close")
	}

	// Verify onTerminated callback was called on EOF, so the session
	// can transition to terminated state when the subprocess crashes.
	if !terminated.Load() {
		t.Error("expected onTerminated callback to be called on EOF")
	}
}

func TestReadLoopInitializedEvent(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), io.Discard)
	go client.ReadLoop()

	// Write an InitializedEvent.
	event := &godap.InitializedEvent{}
	event.Type = "event"
	event.Event.Event = "initialized"
	if err := godap.WriteProtocolMessage(clientReadPW, event); err != nil {
		t.Fatalf("failed to write InitializedEvent: %v", err)
	}

	// Verify it's received on InitializedChan.
	select {
	case <-client.InitializedChan():
		// Expected.
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for InitializedEvent on InitializedChan")
	}
}

func TestReadLoopOutputEvent(t *testing.T) {
	clientReadPR, clientReadPW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
	})

	client := NewClient(bufio.NewReader(clientReadPR), io.Discard)

	outputCh := make(chan *godap.OutputEvent, 1)
	client.SetOutputHandler(func(event *godap.OutputEvent) {
		outputCh <- event
	})

	go client.ReadLoop()

	// Write an OutputEvent.
	event := &godap.OutputEvent{}
	event.Type = "event"
	event.Event.Event = "output"
	event.Body = godap.OutputEventBody{
		Category: "stderr",
		Output:   "error message\n",
	}
	if err := godap.WriteProtocolMessage(clientReadPW, event); err != nil {
		t.Fatalf("failed to write OutputEvent: %v", err)
	}

	// Verify the output handler callback is invoked.
	select {
	case received := <-outputCh:
		if received.Body.Category != "stderr" {
			t.Errorf("Category: got %q, want %q", received.Body.Category, "stderr")
		}
		if received.Body.Output != "error message\n" {
			t.Errorf("Output: got %q, want %q", received.Body.Output, "error message\n")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for OutputEvent callback")
	}
}
