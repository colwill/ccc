// ccc's go probe - records which functions ran on behalf of which test fixture and how many times, as ccc-observed/1 lines
// off unless CCC_OBSERVED_DIR names a directory - `ccc probe go` puts the calls into a copy of the module and this package beside it

package cccprobe

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

const schema = "ccc-observed/1"

// how often a service writes what its fixtures ran - a process killed rather than stopped loses at most this much
const flushEvery = 2 * time.Second

// where records go - nothing is recorded without it
var dir = os.Getenv("CCC_OBSERVED_DIR")

// Fixture is one test, named the way its runner selects it
type Fixture struct {
	Name, File, Language, Kind string
}

type site struct {
	file, name string
	line       int
}

var (
	mu sync.Mutex
	// the fixture each goroutine works for - go keeps no goroutine-local storage, so it is kept by goroutine id
	bound = map[int64]Fixture{}
	// what a goroutine nobody bound inherited from the goroutine that started it - nil when there was nothing to inherit
	inherited = map[int64]*Fixture{}
	// what each fixture ran in this process that is not written yet
	ran = map[Fixture]map[site]int{}
	// this process's own tests still running - each writes its record when it ends
	local = map[Fixture]int{}
	// one record written at a time, so lines from concurrent fixtures never interleave
	writing sync.Mutex
	ticking sync.Once
)

func init() {
	if dir != "" {
		http.DefaultTransport = Transport(http.DefaultTransport)
	}
}

// Hit counts one run of a function for the fixture this goroutine works for, if it works for one
func Hit(file, name string, line int) {
	if dir == "" {
		return
	}
	ticking.Do(func() { go tick() })
	f, ok := current(goid())
	if !ok {
		return
	}
	mu.Lock()
	defer mu.Unlock()
	if ran[f] == nil {
		ran[f] = map[site]int{}
	}
	ran[f][site{file, name, line}]++
}

// Test makes a go test a fixture until it and its subtests end - its record is written then
func Test(t interface {
	Name() string
	Cleanup(func())
}, file, kind string) {
	if dir == "" {
		return
	}
	f := Fixture{Name: t.Name(), File: file, Language: "go", Kind: kind}
	unbind := Bind(f)
	mu.Lock()
	local[f]++
	mu.Unlock()
	start := time.Now()
	t.Cleanup(func() {
		unbind()
		ms := time.Since(start).Milliseconds()
		mu.Lock()
		if local[f]--; local[f] <= 0 {
			delete(local, f)
		}
		hits := ran[f]
		delete(ran, f)
		mu.Unlock()
		write(f, hits, &ms)
	})
}

// Handler binds each request to the fixture its baggage names, for as long as the request is served - nil serves http.DefaultServeMux
func Handler(h http.Handler) http.Handler {
	if dir == "" {
		return h
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if f, ok := FromBaggage(r.Header.Get("Baggage")); ok {
			defer Bind(f)()
		}
		if h == nil {
			http.DefaultServeMux.ServeHTTP(w, r)
			return
		}
		h.ServeHTTP(w, r)
	})
}

// Transport carries the fixture this goroutine works for on every request it sends - nil wraps http.DefaultTransport
func Transport(base http.RoundTripper) http.RoundTripper {
	if dir == "" {
		return base
	}
	if _, done := base.(transport); done {
		return base
	}
	return transport{base: base}
}

// Flush writes what fixtures ran in this process that no test of its own ended - a service's share of a suite driven from outside
func Flush() {
	if dir == "" {
		return
	}
	mu.Lock()
	pending := map[Fixture]map[site]int{}
	for f, hits := range ran {
		if local[f] == 0 {
			pending[f] = hits
			delete(ran, f)
		}
	}
	mu.Unlock()
	for f, hits := range pending {
		write(f, hits, nil)
	}
}

// Bind makes this goroutine work for f until the func it answers is called
func Bind(f Fixture) func() {
	id := goid()
	mu.Lock()
	prev, had := bound[id]
	bound[id] = f
	mu.Unlock()
	return func() {
		mu.Lock()
		defer mu.Unlock()
		if had {
			bound[id] = prev
		} else {
			delete(bound, id)
		}
	}
}

// Current is the fixture this goroutine works for
func Current() (Fixture, bool) {
	if dir == "" {
		return Fixture{}, false
	}
	return current(goid())
}

// Baggage is f as w3c baggage - the same four members every ccc probe sends
func (f Fixture) Baggage() string {
	return strings.Join([]string{
		"ccc.fixture=" + url.QueryEscape(f.Name),
		"ccc.file=" + url.QueryEscape(f.File),
		"ccc.lang=" + url.QueryEscape(f.Language),
		"ccc.kind=" + url.QueryEscape(f.Kind),
	}, ",")
}

// FromBaggage reads the fixture a baggage header names, among whatever other members it carries
func FromBaggage(header string) (Fixture, bool) {
	var f Fixture
	for _, member := range strings.Split(header, ",") {
		member, _, _ = strings.Cut(member, ";")
		k, v, ok := strings.Cut(strings.TrimSpace(member), "=")
		if !ok {
			continue
		}
		v, err := url.QueryUnescape(strings.TrimSpace(v))
		if err != nil {
			continue
		}
		switch k {
		case "ccc.fixture":
			f.Name = v
		case "ccc.file":
			f.File = v
		case "ccc.lang":
			f.Language = v
		case "ccc.kind":
			f.Kind = v
		}
	}
	return f, f.Name != "" && f.Kind != ""
}

// carries the fixture this goroutine works for on to the next service
type transport struct {
	base http.RoundTripper
}

func (t transport) RoundTrip(r *http.Request) (*http.Response, error) {
	base := t.base
	if base == nil {
		base = http.DefaultTransport
	}
	if f, ok := Current(); ok {
		r = r.Clone(r.Context())
		r.Header.Set("Baggage", merge(r.Header.Get("Baggage"), f.Baggage()))
	}
	return base.RoundTrip(r)
}

// ccc's members in place of any earlier ones, beside whatever else the header carries
func merge(header, ours string) string {
	var kept []string
	for _, member := range strings.Split(header, ",") {
		if m := strings.TrimSpace(member); m != "" && !strings.HasPrefix(m, "ccc.") {
			kept = append(kept, m)
		}
	}
	return strings.Join(append(kept, ours), ",")
}

// the fixture goroutine id works for - bound to one, inherited from the goroutine that started it, or this process's one running test
func current(id int64) (Fixture, bool) {
	mu.Lock()
	if f, ok := bound[id]; ok {
		mu.Unlock()
		return f, true
	}
	got, seen := inherited[id]
	mu.Unlock()
	if !seen {
		parent, ok := creator()
		mu.Lock()
		if ok {
			if f, ok := bound[parent]; ok {
				got = &f
			} else if f := inherited[parent]; f != nil {
				got = f
			}
		}
		inherited[id] = got
		mu.Unlock()
	}
	if got != nil {
		return *got, true
	}
	return only()
}

// the one test this process is running - work no request or parent claims is that test's, and with two or more it is nobody's
func only() (Fixture, bool) {
	mu.Lock()
	defer mu.Unlock()
	if len(local) != 1 {
		return Fixture{}, false
	}
	for f := range local {
		return f, true
	}
	return Fixture{}, false
}

// the running goroutine's id - read off the first line of its own stack
func goid() int64 {
	var buf [64]byte
	s := strings.TrimPrefix(string(buf[:runtime.Stack(buf[:], false)]), "goroutine ")
	id, _ := strconv.ParseInt(s[:strings.IndexByte(s, ' ')], 10, 64)
	return id
}

// the goroutine that started this one - go 1.21 and later name it on the last line of the stack
func creator() (int64, bool) {
	buf := make([]byte, 16<<10)
	for {
		n := runtime.Stack(buf, false)
		if n < len(buf) {
			buf = buf[:n]
			break
		}
		buf = make([]byte, len(buf)*2)
	}
	s := string(buf)
	at := strings.LastIndex(s, "\ncreated by ")
	if at < 0 {
		return 0, false
	}
	line := s[at+1:]
	if nl := strings.IndexByte(line, '\n'); nl >= 0 {
		line = line[:nl]
	}
	at = strings.LastIndex(line, " in goroutine ")
	if at < 0 {
		return 0, false
	}
	id, err := strconv.ParseInt(strings.TrimSpace(line[at+len(" in goroutine "):]), 10, 64)
	return id, err == nil
}

// a service's records written as it runs - a test binary's tests write their own as they end
func tick() {
	for range time.Tick(flushEvery) {
		Flush()
	}
}

type function struct {
	Name  string `json:"name"`
	Line  int    `json:"line"`
	Calls int    `json:"calls"`
}

type file struct {
	Path      string     `json:"path"`
	Functions []function `json:"functions"`
}

// one ccc-observed/1 line - a fixture, and the functions it ran in this process with how many times each ran
func write(f Fixture, hits map[site]int, ms *int64) {
	if len(hits) == 0 && ms == nil {
		return
	}
	byFile := map[string][]function{}
	for s, n := range hits {
		byFile[s.file] = append(byFile[s.file], function{Name: s.name, Line: s.line, Calls: n})
	}
	files := make([]file, 0, len(byFile))
	for path, fns := range byFile {
		sort.Slice(fns, func(i, j int) bool { return fns[i].Line < fns[j].Line })
		files = append(files, file{Path: path, Functions: fns})
	}
	sort.Slice(files, func(i, j int) bool { return files[i].Path < files[j].Path })
	service := map[string]string{"name": serviceName()}
	if c := env("CCC_COMMIT", commit); c != "" {
		service["commit"] = c
	}
	rec := map[string]interface{}{
		"schema":  schema,
		"fixture": map[string]string{"name": f.Name, "kind": f.Kind, "file": f.File, "language": f.Language},
		"run":     env("CCC_RUN", "local"),
		"service": service,
		"files":   files,
	}
	if ms != nil {
		rec["duration_ms"] = *ms
	}
	line, err := json.Marshal(rec)
	if err != nil {
		return
	}
	writing.Lock()
	defer writing.Unlock()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return
	}
	out, err := os.OpenFile(filepath.Join(dir, fmt.Sprintf("%s-%d.jsonl", serviceName(), os.Getpid())), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return
	}
	defer out.Close()
	_, _ = out.Write(append(line, '\n'))
}

func serviceName() string {
	return env("CCC_SERVICE", strings.TrimSuffix(filepath.Base(os.Args[0]), ".test"))
}

func env(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}
