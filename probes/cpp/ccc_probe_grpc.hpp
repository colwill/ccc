// carries ccc's fixture through grpc++ - `ccc probe cpp` includes this right after the grpcpp include in the files that use it
// a server binds the fixture on the thread that runs each call, so synchronous services are followed and callback ones are not
#ifndef CCC_PROBE_GRPC_HPP
#define CCC_PROBE_GRPC_HPP

#include <grpcpp/grpcpp.h>
#include <grpcpp/support/client_interceptor.h>
#include <grpcpp/support/server_interceptor.h>

#include "ccc_probe.hpp"

namespace ccc_probe {
namespace detail {

class GrpcServerInterceptor : public grpc::experimental::Interceptor {
 public:
  void Intercept(grpc::experimental::InterceptorBatchMethods* methods) override {
    using grpc::experimental::InterceptionHookPoints;
    if (methods->QueryInterceptionHookPoint(InterceptionHookPoints::POST_RECV_INITIAL_METADATA)) {
      if (auto* md = methods->GetRecvInitialMetadata()) {
        auto it = md->find("baggage");
        if (it != md->end()) bound_ = std::make_unique<Bind>(from_baggage(std::string(it->second.data(), it->second.size())));
      }
    }
    if (methods->QueryInterceptionHookPoint(InterceptionHookPoints::PRE_SEND_STATUS)) bound_.reset();
    methods->Proceed();
  }

 private:
  std::unique_ptr<Bind> bound_;
};

class GrpcServerFactory : public grpc::experimental::ServerInterceptorFactoryInterface {
 public:
  grpc::experimental::Interceptor* CreateServerInterceptor(grpc::experimental::ServerRpcInfo*) override { return new GrpcServerInterceptor(); }
};

class GrpcClientInterceptor : public grpc::experimental::Interceptor {
 public:
  void Intercept(grpc::experimental::InterceptorBatchMethods* methods) override {
    if (methods->QueryInterceptionHookPoint(grpc::experimental::InterceptionHookPoints::PRE_SEND_INITIAL_METADATA)) {
      if (FixtureRef f = current()) methods->GetSendInitialMetadata()->insert({"baggage", baggage(*f)});
    }
    methods->Proceed();
  }
};

class GrpcClientFactory : public grpc::experimental::ClientInterceptorFactoryInterface {
 public:
  grpc::experimental::Interceptor* CreateClientInterceptor(grpc::experimental::ClientRpcInfo*) override { return new GrpcClientInterceptor(); }
};

inline std::vector<std::unique_ptr<grpc::experimental::ClientInterceptorFactoryInterface>> client_factories() {
  std::vector<std::unique_ptr<grpc::experimental::ClientInterceptorFactoryInterface>> out;
  out.push_back(std::make_unique<GrpcClientFactory>());
  return out;
}

}  // namespace detail

// a server builder whose server binds each call to the fixture its metadata names - this replaces interceptors set before it
template <class Builder>
Builder& grpc_server(Builder& builder) {
  std::vector<std::unique_ptr<grpc::experimental::ServerInterceptorFactoryInterface>> creators;
  creators.push_back(std::make_unique<detail::GrpcServerFactory>());
  builder.experimental().SetInterceptorCreators(std::move(creators));
  return builder;
}

// grpc::CreateChannel, with every call carrying the caller's fixture
inline std::shared_ptr<grpc::Channel> grpc_channel(const grpc::string& target, const std::shared_ptr<grpc::ChannelCredentials>& creds) {
  return grpc::experimental::CreateCustomChannelWithInterceptors(target, creds, grpc::ChannelArguments(), detail::client_factories());
}

// grpc::CreateCustomChannel, with every call carrying the caller's fixture
inline std::shared_ptr<grpc::Channel> grpc_custom_channel(const grpc::string& target, const std::shared_ptr<grpc::ChannelCredentials>& creds, const grpc::ChannelArguments& args) {
  return grpc::experimental::CreateCustomChannelWithInterceptors(target, creds, args, detail::client_factories());
}

}  // namespace ccc_probe

#endif
