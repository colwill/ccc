// carries ccc's fixture through cpp-httplib - `ccc probe cpp` includes this right after httplib.h in the files that use it
#ifndef CCC_PROBE_HTTPLIB_HPP
#define CCC_PROBE_HTTPLIB_HPP

#include "ccc_probe.hpp"

namespace ccc_probe {

// a server handler that serves each request for the fixture its baggage names
inline httplib::Server::Handler httplib_serve(httplib::Server::Handler h) {
  return [h = std::move(h)](const httplib::Request& req, httplib::Response& res) {
    Bind b(from_baggage(req.get_header_value("baggage")));
    h(req, res);
  };
}

inline httplib::Server::HandlerWithContentReader httplib_serve(httplib::Server::HandlerWithContentReader h) {
  return [h = std::move(h)](const httplib::Request& req, httplib::Response& res, const httplib::ContentReader& reader) {
    Bind b(from_baggage(req.get_header_value("baggage")));
    h(req, res, reader);
  };
}

// a client whose every request carries the fixture it is sent for
template <class Client>
Client& httplib_client(Client& client) {
  client.set_header_writer([](httplib::Stream& strm, httplib::Headers& headers) {
    if (FixtureRef f = current()) {
      headers.erase("baggage");
      headers.emplace("baggage", baggage(*f));
    }
    return httplib::detail::write_headers(strm, headers);
  });
  return client;
}

}  // namespace ccc_probe

#endif
