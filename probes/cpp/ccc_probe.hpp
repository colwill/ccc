// ccc's c++ probe - records which functions ran on behalf of which test fixture and how many times, as ccc-observed/1 lines
// off unless CCC_OBSERVED_DIR names a directory - `ccc probe cpp` includes this header into a copy of the project, c++17 or later
#ifndef CCC_PROBE_HPP
#define CCC_PROBE_HPP

#include <cctype>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>
#if defined(_WIN32)
#include <process.h>
#else
#include <unistd.h>
#endif

namespace ccc_probe {

// one test, named the way its runner selects it
struct Fixture {
  std::string name, file, language, kind;
};
using FixtureRef = std::shared_ptr<const Fixture>;

namespace detail {

// the commit the copy was made from - CCC_COMMIT overrides it
inline const char* commit() { return ""; }

// how often a service writes what its fixtures ran - a process killed rather than stopped loses at most this much
constexpr int flush_seconds = 2;

struct Site {
  const char* file;
  const char* name;
  int line;
  bool operator==(const Site& o) const { return file == o.file && name == o.name && line == o.line; }
};

struct SiteHash {
  std::size_t operator()(const Site& s) const {
    return std::hash<const void*>()(s.file) ^ (std::hash<const void*>()(s.name) << 1) ^ (std::hash<int>()(s.line) << 2);
  }
};

struct Entry {
  FixtureRef fixture;
  std::unordered_map<Site, std::uint64_t, SiteHash> sites;
};

// never destroyed, so a detached flusher or a static destructor can still reach it as the process ends
struct State {
  std::mutex mu;
  std::mutex writing;
  // what each fixture ran that is not written yet
  std::map<std::string, Entry> ran;
  // this process's own tests still running - each writes its record when it ends
  std::map<std::string, std::pair<FixtureRef, int>> local;
  bool ticking = false;
};

inline State& state() {
  static State* s = new State();
  return *s;
}

inline const char* dir() {
  static const char* d = [] {
    const char* v = std::getenv("CCC_OBSERVED_DIR");
    return (v && *v) ? v : nullptr;
  }();
  return d;
}

inline FixtureRef& bound() {
  thread_local FixtureRef f;
  return f;
}

inline std::string key(const Fixture& f) { return f.name + '\0' + f.file + '\0' + f.language + '\0' + f.kind; }

inline std::string json(const std::string& s) {
  std::string out = "\"";
  for (unsigned char c : s) {
    if (c == '"' || c == '\\') {
      out += '\\';
      out += static_cast<char>(c);
    } else if (c < 0x20) {
      char buf[8];
      std::snprintf(buf, sizeof buf, "\\u%04x", c);
      out += buf;
    } else {
      out += static_cast<char>(c);
    }
  }
  return out + "\"";
}

inline std::string encode(const std::string& s) {
  static const char* hex = "0123456789ABCDEF";
  std::string out;
  for (unsigned char c : s) {
    if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' || c == '_' || c == '.' || c == '~') {
      out += static_cast<char>(c);
    } else {
      out += '%';
      out += hex[c >> 4];
      out += hex[c & 15];
    }
  }
  return out;
}

inline std::string decode(const std::string& s) {
  std::string out;
  for (std::size_t i = 0; i < s.size(); ++i) {
    if (s[i] == '+') {
      out += ' ';
    } else if (s[i] == '%' && i + 2 < s.size() && std::isxdigit(static_cast<unsigned char>(s[i + 1])) && std::isxdigit(static_cast<unsigned char>(s[i + 2]))) {
      out += static_cast<char>(std::stoi(s.substr(i + 1, 2), nullptr, 16));
      i += 2;
    } else {
      out += s[i];
    }
  }
  return out;
}

inline std::string env(const char* name, const std::string& fallback) {
  const char* v = std::getenv(name);
  return (v && *v) ? std::string(v) : fallback;
}

inline std::string service() {
  std::string name = env("CCC_SERVICE", "");
  if (!name.empty()) return name;
#if defined(__linux__)
  if (std::FILE* f = std::fopen("/proc/self/comm", "r")) {
    char buf[256] = {0};
    if (std::fgets(buf, sizeof buf, f)) name = buf;
    std::fclose(f);
    while (!name.empty() && (name.back() == '\n' || name.back() == '\r')) name.pop_back();
  }
#endif
  return name.empty() ? "cpp" : name;
}

inline long pid() {
#if defined(_WIN32)
  return static_cast<long>(_getpid());
#else
  return static_cast<long>(getpid());
#endif
}

// one ccc-observed/1 line - a fixture, and the functions it ran in this process with how many times each ran
inline void write(const Fixture& f, const std::unordered_map<Site, std::uint64_t, SiteHash>& sites, long long ms) {
  if (sites.empty() && ms < 0) return;
  // the same literal can sit at two addresses in two translation units, so sites merge by what they say
  std::map<std::string, std::map<std::pair<int, std::string>, std::uint64_t>> files;
  for (const auto& [site, calls] : sites) files[site.file][{site.line, site.name}] += calls;
  std::string out = "{\"schema\":\"ccc-observed/1\",\"fixture\":{\"name\":" + json(f.name) + ",\"kind\":" + json(f.kind) + ",\"file\":" + json(f.file) +
                    ",\"language\":" + json(f.language) + "},\"run\":" + json(env("CCC_RUN", "local")) + ",\"service\":{\"name\":" + json(service());
  std::string at = env("CCC_COMMIT", commit());
  if (!at.empty()) out += ",\"commit\":" + json(at);
  out += "},\"files\":[";
  bool first_file = true;
  for (const auto& [path, fns] : files) {
    out += first_file ? "" : ",";
    first_file = false;
    out += "{\"path\":" + json(path) + ",\"functions\":[";
    bool first = true;
    for (const auto& [where, calls] : fns) {
      out += first ? "" : ",";
      first = false;
      out += "{\"name\":" + json(where.second) + ",\"line\":" + std::to_string(where.first) + ",\"calls\":" + std::to_string(calls) + "}";
    }
    out += "]}";
  }
  out += "]";
  if (ms >= 0) out += ",\"duration_ms\":" + std::to_string(ms);
  out += "}\n";
  std::lock_guard<std::mutex> lock(state().writing);
  std::error_code ignored;
  std::filesystem::create_directories(dir(), ignored);
  std::string path = std::string(dir()) + "/" + service() + "-" + std::to_string(pid()) + ".jsonl";
  if (std::FILE* file = std::fopen(path.c_str(), "a")) {
    std::fwrite(out.data(), 1, out.size(), file);
    std::fclose(file);
  }
}

}  // namespace detail

// writes what fixtures ran in this process that no test of its own is still running
inline void flush() {
  if (!detail::dir()) return;
  std::vector<detail::Entry> pending;
  {
    auto& s = detail::state();
    std::lock_guard<std::mutex> lock(s.mu);
    for (auto it = s.ran.begin(); it != s.ran.end();) {
      if (s.local.count(it->first)) {
        ++it;
        continue;
      }
      pending.push_back(std::move(it->second));
      it = s.ran.erase(it);
    }
  }
  for (const auto& e : pending) detail::write(*e.fixture, e.sites, -1);
}

namespace detail {

inline void tick() {
  auto& s = state();
  {
    std::lock_guard<std::mutex> lock(s.mu);
    if (s.ticking) return;
    s.ticking = true;
  }
  std::thread([] {
    for (;;) {
      std::this_thread::sleep_for(std::chrono::seconds(flush_seconds));
      flush();
    }
  }).detach();
}

// what fixtures ran is written when the process exits normally
struct AtExit {
  ~AtExit() { flush(); }
};
inline AtExit at_exit;

}  // namespace detail

// the fixture this work runs for - bound to this thread, or this process's one running test
inline FixtureRef current() {
  if (!detail::dir()) return nullptr;
  if (FixtureRef f = detail::bound()) return f;
  auto& s = detail::state();
  std::lock_guard<std::mutex> lock(s.mu);
  if (s.local.size() == 1) return s.local.begin()->second.first;
  return nullptr;
}

// counts one run of a function for the fixture this work runs for
inline void hit(const char* file, const char* name, int line) {
  if (!detail::dir()) return;
  detail::tick();
  FixtureRef f = current();
  if (!f) return;
  auto& s = detail::state();
  std::lock_guard<std::mutex> lock(s.mu);
  auto& e = s.ran[detail::key(*f)];
  if (!e.fixture) e.fixture = f;
  ++e.sites[detail::Site{file, name, line}];
}

// makes this thread work for a fixture until it goes out of scope - a null fixture leaves whatever was bound
class Bind {
 public:
  explicit Bind(FixtureRef f) : active_(f != nullptr) {
    if (active_) prev_ = std::exchange(detail::bound(), std::move(f));
  }
  ~Bind() {
    if (active_) detail::bound() = std::move(prev_);
  }
  Bind(const Bind&) = delete;
  Bind& operator=(const Bind&) = delete;

 private:
  bool active_;
  FixtureRef prev_;
};

// a test as a fixture until it goes out of scope - its record is written then
class Test {
 public:
  Test(const char* name, const char* file, const char* kind) {
    if (!detail::dir()) return;
    fixture_ = std::make_shared<const Fixture>(Fixture{name, file, "cpp", kind});
    {
      auto& s = detail::state();
      std::lock_guard<std::mutex> lock(s.mu);
      auto& open = s.local[detail::key(*fixture_)];
      open.first = fixture_;
      ++open.second;
    }
    bind_ = std::make_unique<Bind>(fixture_);
    start_ = std::chrono::steady_clock::now();
  }
  ~Test() {
    if (!fixture_) return;
    bind_.reset();
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - start_).count();
    detail::Entry entry;
    {
      auto& s = detail::state();
      std::lock_guard<std::mutex> lock(s.mu);
      std::string k = detail::key(*fixture_);
      auto open = s.local.find(k);
      if (open != s.local.end() && --open->second.second <= 0) s.local.erase(open);
      auto ran = s.ran.find(k);
      if (ran != s.ran.end()) {
        entry = std::move(ran->second);
        s.ran.erase(ran);
      }
    }
    detail::write(*fixture_, entry.sites, ms);
  }
  Test(const Test&) = delete;
  Test& operator=(const Test&) = delete;

 private:
  FixtureRef fixture_;
  std::unique_ptr<Bind> bind_;
  std::chrono::steady_clock::time_point start_;
};

inline Test fixture(const char* name, const char* file, const char* kind) { return Test(name, file, kind); }

// the fixture as w3c baggage - the same four members every ccc probe sends
inline std::string baggage(const Fixture& f) {
  return "ccc.fixture=" + detail::encode(f.name) + ",ccc.file=" + detail::encode(f.file) + ",ccc.lang=" + detail::encode(f.language) +
         ",ccc.kind=" + detail::encode(f.kind);
}

// the fixture a baggage header names, among whatever other members it carries
inline FixtureRef from_baggage(const std::string& header) {
  Fixture f;
  std::size_t start = 0;
  while (start <= header.size()) {
    std::size_t end = header.find(',', start);
    std::string member = header.substr(start, end == std::string::npos ? std::string::npos : end - start);
    member = member.substr(0, member.find(';'));
    std::size_t eq = member.find('=');
    if (eq != std::string::npos) {
      std::string k = member.substr(0, eq), v = member.substr(eq + 1);
      auto trim = [](std::string& t) {
        t.erase(0, t.find_first_not_of(" \t"));
        t.erase(t.find_last_not_of(" \t") + 1);
      };
      trim(k);
      trim(v);
      v = detail::decode(v);
      if (k == "ccc.fixture") f.name = v;
      if (k == "ccc.file") f.file = v;
      if (k == "ccc.lang") f.language = v;
      if (k == "ccc.kind") f.kind = v;
    }
    if (end == std::string::npos) break;
    start = end + 1;
  }
  if (f.name.empty() || f.kind.empty()) return nullptr;
  return std::make_shared<const Fixture>(std::move(f));
}

// a callable handed to another thread carries the fixture of whoever handed it over
template <class F>
auto bind_current(F fn) {
  return [f = current(), fn = std::move(fn)](auto&&... args) mutable -> decltype(auto) {
    Bind b(f);
    return std::invoke(fn, std::forward<decltype(args)>(args)...);
  };
}

}  // namespace ccc_probe

#endif
