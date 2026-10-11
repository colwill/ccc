// carries ccc's fixture over grpc - `ccc probe go` writes this into a module only when it requires google.golang.org/grpc
package cccprobegrpc

import (
	"context"
	"strings"

	cccprobe "ccc.invalid/probe"
	"google.golang.org/grpc"
	"google.golang.org/grpc/metadata"
)

// UnaryServer binds each unary call a server takes to the fixture its metadata names
func UnaryServer() grpc.ServerOption {
	return grpc.ChainUnaryInterceptor(func(ctx context.Context, req interface{}, info *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (interface{}, error) {
		if f, ok := incoming(ctx); ok {
			defer cccprobe.Bind(f)()
		}
		return handler(ctx, req)
	})
}

// StreamServer binds each stream a server takes to the fixture its metadata names
func StreamServer() grpc.ServerOption {
	return grpc.ChainStreamInterceptor(func(srv interface{}, ss grpc.ServerStream, info *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
		if f, ok := incoming(ss.Context()); ok {
			defer cccprobe.Bind(f)()
		}
		return handler(srv, ss)
	})
}

// UnaryClient carries the caller's fixture on every unary call
func UnaryClient() grpc.DialOption {
	return grpc.WithChainUnaryInterceptor(func(ctx context.Context, method string, req, reply interface{}, cc *grpc.ClientConn, invoker grpc.UnaryInvoker, opts ...grpc.CallOption) error {
		return invoker(outgoing(ctx), method, req, reply, cc, opts...)
	})
}

// StreamClient carries the caller's fixture on every stream it opens
func StreamClient() grpc.DialOption {
	return grpc.WithChainStreamInterceptor(func(ctx context.Context, desc *grpc.StreamDesc, cc *grpc.ClientConn, method string, streamer grpc.Streamer, opts ...grpc.CallOption) (grpc.ClientStream, error) {
		return streamer(outgoing(ctx), desc, cc, method, opts...)
	})
}

func incoming(ctx context.Context) (cccprobe.Fixture, bool) {
	md, ok := metadata.FromIncomingContext(ctx)
	if !ok {
		return cccprobe.Fixture{}, false
	}
	return cccprobe.FromBaggage(strings.Join(md.Get("baggage"), ","))
}

func outgoing(ctx context.Context) context.Context {
	f, ok := cccprobe.Current()
	if !ok {
		return ctx
	}
	return metadata.AppendToOutgoingContext(ctx, "baggage", f.Baggage())
}
