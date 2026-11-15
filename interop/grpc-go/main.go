// Interop checks between ferroly's gRPC stack and grpc-go.
//
//	go run . server <port>   a grpc-go server for ferroly's interop client
//	go run . client <addr>   checks a ferroly server (crates/ferroly/examples/grpc_interop.rs)
//
// Both sides implement the same service, "/p.S/*", whose messages are a
// single `bytes` field (tag 1). No generated code is needed: a pass-through
// codec sends the protobuf bytes as they are. The client prints one PASS or
// FAIL line per check and exits with status 1 if any check failed.
//
// Run both directions with interop/grpc-go.sh.
package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net"
	"os"
	"strings"
	"time"

	spb "google.golang.org/genproto/googleapis/rpc/status"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/encoding"
	_ "google.golang.org/grpc/encoding/gzip"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/anypb"
)

// The error message both servers return for "err"; it exercises the
// percent-encoding of grpc-message (non-ASCII, '%', control characters).
const errMessage = "bad ✓ 100% \r\n\t end"

// The type URL of the single Any detail the ferroly server attaches to that error.
const detailTypeURL = "type.googleapis.com/p.Detail"

// rawCodec passes *[]byte through unchanged (already-encoded protobuf) and
// encodes anything else as a proto message (the health service).
type rawCodec struct{}

func (rawCodec) Marshal(v any) ([]byte, error) {
	if b, ok := v.(*[]byte); ok {
		return *b, nil
	}
	return proto.Marshal(v.(proto.Message))
}

func (rawCodec) Unmarshal(data []byte, v any) error {
	if b, ok := v.(*[]byte); ok {
		*b = append([]byte(nil), data...)
		return nil
	}
	return proto.Unmarshal(data, v.(proto.Message))
}

func (rawCodec) Name() string { return "proto" }

func init() { encoding.RegisterCodec(rawCodec{}) }

// enc encodes s as field 1 (bytes) of a message.
func enc(s string) []byte {
	b := []byte{0x0a}
	n := len(s)
	for n >= 0x80 {
		b = append(b, byte(n)|0x80)
		n >>= 7
	}
	b = append(b, byte(n))
	return append(b, s...)
}

// dec decodes field 1 of a message produced by enc (empty for an empty message).
func dec(b []byte) string {
	if len(b) < 2 {
		return ""
	}
	i, n, shift := 1, 0, 0
	for i < len(b) {
		c := b[i]
		i++
		n |= int(c&0x7f) << shift
		shift += 7
		if c < 0x80 {
			break
		}
	}
	if i+n > len(b) {
		return "<malformed>"
	}
	return string(b[i : i+n])
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: grpc-go (server <port> | client <host:port>)")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "server":
		server(os.Args[2])
	case "client":
		os.Exit(client(os.Args[2]))
	default:
		fmt.Fprintln(os.Stderr, "usage: grpc-go (server <port> | client <host:port>)")
		os.Exit(2)
	}
}

// ---- client: checks a ferroly server ---------------------------------------

type checker struct{ failed int }

func (c *checker) check(name string, ok bool, format string, args ...any) {
	detail := fmt.Sprintf(format, args...)
	if ok {
		fmt.Printf("PASS %s: %s\n", name, detail)
		return
	}
	c.failed++
	fmt.Printf("FAIL %s: %s\n", name, detail)
}

func describe(err error) string {
	if err == nil {
		return "OK"
	}
	st, _ := status.FromError(err)
	return fmt.Sprintf("code=%v msg=%q", st.Code(), st.Message())
}

func codeOf(err error) codes.Code { return status.Code(err) }

func client(addr string) int {
	conn, err := grpc.NewClient(addr, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		fmt.Printf("FAIL connect: %v\n", err)
		return 1
	}
	defer conn.Close()
	c := &checker{}
	bg := context.Background()
	ctx := func() (context.Context, context.CancelFunc) { return context.WithTimeout(bg, 10*time.Second) }

	// Unary with ASCII and binary request metadata, response headers and trailers.
	{
		cx, cancel := ctx()
		md := metadata.Pairs("x-a", "1", "x-b-bin", string([]byte{0, 1, 0xff}))
		cx = metadata.NewOutgoingContext(cx, md)
		var hdr, trl metadata.MD
		in, out := enc("hello"), []byte(nil)
		err := conn.Invoke(cx, "/p.S/Unary", &in, &out, grpc.Header(&hdr), grpc.Trailer(&trl))
		cancel()
		c.check("unary", err == nil && dec(out) == "hello", "%s resp=%q", describe(err), dec(out))
		c.check("unary header", strings.Join(hdr.Get("x-h"), ",") == "hv", "x-h=%v", hdr.Get("x-h"))
		c.check("unary binary trailer", strings.Join(trl.Get("x-t-bin"), ",") == "\x09\x09", "x-t-bin=%q", trl.Get("x-t-bin"))
	}

	// An error status with a non-ASCII message, trailers and details.
	{
		cx, cancel := ctx()
		var trl metadata.MD
		in, out := enc("err"), []byte(nil)
		err := conn.Invoke(cx, "/p.S/Unary", &in, &out, grpc.Trailer(&trl))
		cancel()
		st, _ := status.FromError(err)
		c.check("unary error status", st.Code() == codes.InvalidArgument && st.Message() == errMessage, "%s", describe(err))
		c.check("unary error trailers",
			strings.Join(trl.Get("x-why"), ",") == "because" && strings.Join(trl.Get("x-why-bin"), ",") == "\x00\x01\x02\xff",
			"x-why=%q x-why-bin=%q", trl.Get("x-why"), trl.Get("x-why-bin"))
		// The server attaches a google.rpc.Status with one Any detail; grpc-go
		// keeps it only when its code matches grpc-status.
		details := st.Proto().GetDetails()
		c.check("unary error details", len(details) == 1 && details[0].GetTypeUrl() == detailTypeURL && string(details[0].GetValue()) == "x",
			"details=%v", details)
	}

	// A gzip-compressed request: ferroly decodes it (and answers identity).
	{
		cx, cancel := ctx()
		in, out := enc("hello"), []byte(nil)
		err := conn.Invoke(cx, "/p.S/Unary", &in, &out, grpc.UseCompressor("gzip"))
		cancel()
		c.check("gzip request", err == nil && dec(out) == "hello", "%s %q", describe(err), dec(out))
	}

	// Server streaming: a clean end, then a failure after the first message.
	for _, arg := range []string{"abc", "midfail"} {
		cx, cancel := ctx()
		s, err := conn.NewStream(cx, &grpc.StreamDesc{ServerStreams: true}, "/p.S/ServerStream")
		if err != nil {
			c.check("server stream "+arg, false, "open: %s", describe(err))
			cancel()
			continue
		}
		in := enc(arg)
		_ = s.SendMsg(&in)
		_ = s.CloseSend()
		var got []string
		var end error
		for {
			var out []byte
			if err := s.RecvMsg(&out); err != nil {
				end = err
				break
			}
			got = append(got, dec(out))
		}
		cancel()
		if arg == "abc" {
			c.check("server stream", end == io.EOF && strings.Join(got, ",") == "m0,m1,m2", "msgs=%v end=%v", got, end)
		} else {
			st, _ := status.FromError(end)
			c.check("server stream mid-failure",
				strings.Join(got, ",") == "one" && st.Code() == codes.Aborted && st.Message() == "mid-stream failure ✓",
				"msgs=%v end=%s", got, describe(end))
		}
	}

	// Client streaming.
	{
		cx, cancel := ctx()
		s, err := conn.NewStream(cx, &grpc.StreamDesc{ClientStreams: true}, "/p.S/ClientStream")
		var out []byte
		if err == nil {
			for _, p := range []string{"a", "b", "c"} {
				in := enc(p)
				_ = s.SendMsg(&in)
			}
			_ = s.CloseSend()
			err = s.RecvMsg(&out)
		}
		cancel()
		c.check("client stream", err == nil && dec(out) == "abc", "%s resp=%q", describe(err), dec(out))
	}

	// Client streaming where the server answers without reading; the client
	// keeps sending and must still get the answer.
	{
		cx, cancel := ctx()
		s, err := conn.NewStream(cx, &grpc.StreamDesc{ClientStreams: true}, "/p.S/ClientStreamEarly")
		var out []byte
		if err == nil {
			done := make(chan struct{})
			go func() {
				defer close(done)
				for i := 0; i < 2000; i++ {
					in := enc(strings.Repeat("x", 1024))
					if s.SendMsg(&in) != nil {
						return
					}
				}
			}()
			err = s.RecvMsg(&out)
			<-done
		}
		cancel()
		c.check("early answer", err == nil && dec(out) == "early", "%s resp=%q", describe(err), dec(out))
	}

	// Bidirectional streaming, one message at a time.
	{
		cx, cancel := ctx()
		s, err := conn.NewStream(cx, &grpc.StreamDesc{ClientStreams: true, ServerStreams: true}, "/p.S/Bidi")
		ok := err == nil
		var got []string
		if ok {
			for _, p := range []string{"p1", "p2"} {
				in := enc(p)
				_ = s.SendMsg(&in)
				var out []byte
				if err := s.RecvMsg(&out); err != nil {
					ok = false
					break
				}
				got = append(got, dec(out))
			}
			_ = s.CloseSend()
			var out []byte
			if s.RecvMsg(&out) != io.EOF {
				ok = false
			}
		}
		cancel()
		c.check("bidi", ok && strings.Join(got, ",") == "p1,p2", "msgs=%v err=%v", got, err)
	}

	// The deadline reaches the server and ends the call in time.
	{
		cx, cancel := context.WithTimeout(bg, 300*time.Millisecond)
		start := time.Now()
		in, out := enc(""), []byte(nil)
		err := conn.Invoke(cx, "/p.S/Sleep", &in, &out)
		cancel()
		elapsed := time.Since(start)
		c.check("deadline", codeOf(err) == codes.DeadlineExceeded && elapsed < 5*time.Second,
			"%s after %v", describe(err), elapsed.Round(time.Millisecond))
	}

	// Unknown method and service; a panicking handler.
	{
		cx, cancel := ctx()
		in, out := enc(""), []byte(nil)
		err := conn.Invoke(cx, "/p.S/Nope", &in, &out)
		c.check("unknown method", codeOf(err) == codes.Unimplemented, "%s", describe(err))
		err = conn.Invoke(cx, "/p.X/Nope", &in, &out)
		c.check("unknown service", codeOf(err) == codes.Unimplemented, "%s", describe(err))
		err = conn.Invoke(cx, "/p.S/Panic", &in, &out)
		c.check("handler panic", codeOf(err) == codes.Internal, "%s", describe(err))
		cancel()
	}

	// Cancelling an endless server stream; the connection stays usable.
	{
		cx, cancel := context.WithCancel(bg)
		s, err := conn.NewStream(cx, &grpc.StreamDesc{ServerStreams: true}, "/p.S/Infinite")
		n := 0
		if err == nil {
			in := enc("x")
			_ = s.SendMsg(&in)
			_ = s.CloseSend()
			for ; n < 3; n++ {
				var out []byte
				if s.RecvMsg(&out) != nil {
					break
				}
			}
		}
		cancel()
		c.check("cancel endless stream", n == 3, "received %d messages before cancelling", n)
		time.Sleep(200 * time.Millisecond)
		cx2, cancel2 := ctx()
		in, out := enc("after"), []byte(nil)
		err = conn.Invoke(cx2, "/p.S/Unary", &in, &out)
		cancel2()
		c.check("call after cancel", err == nil && dec(out) == "after", "%s", describe(err))
	}

	// The standard health service.
	{
		hc := healthpb.NewHealthClient(conn)
		cx, cancel := ctx()
		r, err := hc.Check(cx, &healthpb.HealthCheckRequest{})
		c.check("health check", err == nil && r.GetStatus() == healthpb.HealthCheckResponse_SERVING, "%v %s", r.GetStatus(), describe(err))
		_, err = hc.Check(cx, &healthpb.HealthCheckRequest{Service: "nope"})
		c.check("health check unknown", codeOf(err) == codes.NotFound, "%s", describe(err))
		cancel()
		cw, cancelw := context.WithTimeout(bg, 2*time.Second)
		w, err := hc.Watch(cw, &healthpb.HealthCheckRequest{Service: "nope"})
		var got healthpb.HealthCheckResponse_ServingStatus
		if err == nil {
			var r *healthpb.HealthCheckResponse
			r, err = w.Recv()
			got = r.GetStatus()
		}
		cancelw()
		c.check("health watch unknown", err == nil && got == healthpb.HealthCheckResponse_SERVICE_UNKNOWN, "%v %s", got, describe(err))
	}

	// Many cancelled calls on one connection (each deadline expiry is an
	// RST_STREAM) must not look like a reset flood: every one of them ends
	// DEADLINE_EXCEEDED, and an unrelated call in flight meanwhile is not cut
	// off by a GOAWAY.
	{
		victim := make(chan error, 1)
		go func() {
			cx, cancel := context.WithTimeout(bg, 4*time.Second)
			defer cancel()
			in, out := enc(""), []byte(nil)
			victim <- conn.Invoke(cx, "/p.S/Sleep", &in, &out)
		}()
		time.Sleep(100 * time.Millisecond)
		const total = 400
		results := make(chan error, total)
		sem := make(chan struct{}, 32)
		for i := 0; i < total; i++ {
			sem <- struct{}{}
			go func() {
				defer func() { <-sem }()
				cx, cancel := context.WithTimeout(bg, 20*time.Millisecond)
				defer cancel()
				in, out := enc(""), []byte(nil)
				results <- conn.Invoke(cx, "/p.S/Sleep", &in, &out)
			}()
		}
		other := map[string]int{}
		for i := 0; i < total; i++ {
			if err := <-results; codeOf(err) != codes.DeadlineExceeded {
				other[describe(err)]++
			}
		}
		c.check("400 cancelled calls", len(other) == 0, "other outcomes: %v", other)
		err := <-victim
		c.check("call in flight during cancellations", codeOf(err) == codes.DeadlineExceeded, "%s", describe(err))
		cx, cancel := ctx()
		in, out := enc("still"), []byte(nil)
		err = conn.Invoke(cx, "/p.S/Unary", &in, &out)
		cancel()
		c.check("call after cancellations", err == nil && bytes.Equal(out, enc("still")), "%s", describe(err))
	}

	if c.failed > 0 {
		fmt.Printf("%d check(s) failed\n", c.failed)
		return 1
	}
	fmt.Println("all checks passed")
	return 0
}

// ---- server: the same service, for ferroly's interop client ----------------

func server(port string) {
	lis, err := net.Listen("tcp", "127.0.0.1:"+port)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	s := grpc.NewServer(grpc.UnknownServiceHandler(handle))
	healthpb.RegisterHealthServer(s, health.NewServer())
	fmt.Fprintf(os.Stderr, "[grpc-go] listening on 127.0.0.1:%s\n", port)
	if err := s.Serve(lis); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func handle(_ any, stream grpc.ServerStream) error {
	m, _ := grpc.MethodFromServerStream(stream)
	switch m {
	case "/p.S/Unary":
		var in []byte
		if err := stream.RecvMsg(&in); err != nil {
			return err
		}
		md, _ := metadata.FromIncomingContext(stream.Context())
		if dec(in) == "err" {
			trl := metadata.Pairs("x-why", "because", "x-why-bin", string([]byte{0, 1, 2, 0xff}))
			// Echo the request's binary metadata back, so the client can
			// check that it arrived intact.
			for _, v := range md.Get("q-bin") {
				trl.Append("q-bin", v)
			}
			stream.SetTrailer(trl)
			return status.FromProto(&spb.Status{
				Code:    int32(codes.InvalidArgument),
				Message: errMessage,
				Details: []*anypb.Any{{TypeUrl: detailTypeURL, Value: []byte("x")}},
			}).Err()
		}
		if dec(in) == "gzip" {
			// Answer compressed: the ferroly client advertises gzip.
			if err := grpc.SetSendCompressor(stream.Context(), "gzip"); err != nil {
				return status.Error(codes.Internal, "SetSendCompressor: "+err.Error())
			}
		}
		_ = stream.SetHeader(metadata.Pairs("x-h", "hv"))
		stream.SetTrailer(metadata.Pairs("x-t-bin", string([]byte{9, 9})))
		return stream.SendMsg(&in)
	case "/p.S/ServerStream":
		var in []byte
		if err := stream.RecvMsg(&in); err != nil {
			return err
		}
		if dec(in) == "midfail" {
			o := enc("one")
			_ = stream.SendMsg(&o)
			return status.Error(codes.Aborted, "mid-stream failure ✓")
		}
		for i := 0; i < len(dec(in)); i++ {
			o := enc(fmt.Sprintf("m%d", i))
			if err := stream.SendMsg(&o); err != nil {
				return err
			}
		}
		return nil
	case "/p.S/ClientStream":
		all := ""
		for {
			var in []byte
			if err := stream.RecvMsg(&in); err == io.EOF {
				break
			} else if err != nil {
				return err
			}
			all += dec(in)
		}
		o := enc(all)
		return stream.SendMsg(&o)
	case "/p.S/ClientStreamEarly":
		o := enc("early")
		return stream.SendMsg(&o)
	case "/p.S/Bidi":
		for {
			var in []byte
			if err := stream.RecvMsg(&in); err == io.EOF {
				return nil
			} else if err != nil {
				return err
			}
			if err := stream.SendMsg(&in); err != nil {
				return err
			}
		}
	case "/p.S/Sleep":
		select {
		case <-stream.Context().Done():
			return status.FromContextError(stream.Context().Err()).Err()
		case <-time.After(60 * time.Second):
		}
		o := enc("")
		return stream.SendMsg(&o)
	case "/p.S/Panic":
		return status.Error(codes.Internal, "the Go server does not panic; INTERNAL stands in")
	case "/p.S/Infinite":
		for {
			o := enc("x")
			if err := stream.SendMsg(&o); err != nil {
				return err
			}
		}
	}
	return status.Error(codes.Unimplemented, "unknown method "+m)
}
