// TechEmpower Framework Benchmarks scenarios on net/http + pgx (the same
// service as ../zell/main.zl): /plaintext, /json, /db, /queries?queries=N,
// /fortunes, /updates?queries=N. Env: PORT (8080), DB_DSN, DB_POOL (64).
package main

import (
	"context"
	"encoding/json"
	"html"
	"log"
	"math/rand/v2"
	"net/http"
	"os"
	"slices"
	"strconv"
	"strings"

	"github.com/jackc/pgx/v5/pgxpool"
)

type Message struct {
	Message string `json:"message"`
}

type World struct {
	Id           int32 `json:"id"`
	RandomNumber int32 `json:"randomNumber"`
}

type Fortune struct {
	Id      int32
	Message string
}

const selectWorld = "select id, randomnumber from world where id = $1"
const updateWorld = "update world set randomnumber = $1 where id = $2"

var db *pgxpool.Pool

func randomId() int32 { return int32(rand.IntN(10000) + 1) }

func queryCount(r *http.Request) int {
	n, err := strconv.Atoi(r.URL.Query().Get("queries"))
	if err != nil || n < 1 {
		return 1
	}
	if n > 500 {
		return 500
	}
	return n
}

func sendJSON(w http.ResponseWriter, v any) {
	b, _ := json.Marshal(v)
	w.Header().Set("Content-Type", "application/json")
	w.Write(b)
}

func plaintext(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.Write([]byte("Hello, World!"))
}

func jsonHandler(w http.ResponseWriter, r *http.Request) {
	sendJSON(w, Message{Message: "Hello, World!"})
}

func loadWorld(ctx context.Context) (World, error) {
	var w World
	err := db.QueryRow(ctx, selectWorld, randomId()).Scan(&w.Id, &w.RandomNumber)
	return w, err
}

func single(w http.ResponseWriter, r *http.Request) {
	wd, err := loadWorld(r.Context())
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}
	sendJSON(w, wd)
}

func queries(w http.ResponseWriter, r *http.Request) {
	n := queryCount(r)
	ws := make([]World, 0, n)
	for i := 0; i < n; i++ {
		wd, err := loadWorld(r.Context())
		if err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		ws = append(ws, wd)
	}
	sendJSON(w, ws)
}

func updates(w http.ResponseWriter, r *http.Request) {
	n := queryCount(r)
	ws := make([]World, 0, n)
	for i := 0; i < n; i++ {
		wd, err := loadWorld(r.Context())
		if err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		wd.RandomNumber = randomId()
		ws = append(ws, wd)
	}
	for _, wd := range ws {
		if _, err := db.Exec(r.Context(), updateWorld, wd.RandomNumber, wd.Id); err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
	}
	sendJSON(w, ws)
}

func fortunes(w http.ResponseWriter, r *http.Request) {
	rows, err := db.Query(r.Context(), "select id, message from fortune")
	if err != nil {
		http.Error(w, err.Error(), 500)
		return
	}
	fs := make([]Fortune, 0, 16)
	for rows.Next() {
		var f Fortune
		if err := rows.Scan(&f.Id, &f.Message); err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		fs = append(fs, f)
	}
	rows.Close()
	fs = append(fs, Fortune{Id: 0, Message: "Additional fortune added at request time."})
	slices.SortFunc(fs, func(a, b Fortune) int { return strings.Compare(a.Message, b.Message) })
	var b strings.Builder
	b.WriteString("<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>")
	for _, f := range fs {
		b.WriteString("<tr><td>")
		b.WriteString(strconv.Itoa(int(f.Id)))
		b.WriteString("</td><td>")
		b.WriteString(html.EscapeString(f.Message))
		b.WriteString("</td></tr>")
	}
	b.WriteString("</table></body></html>")
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Write([]byte(b.String()))
}

func main() {
	dsn := os.Getenv("DB_DSN")
	if dsn == "" {
		dsn = "postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1:5432/hello_world?sslmode=disable"
	}
	pool := 64
	if s := os.Getenv("DB_POOL"); s != "" {
		pool, _ = strconv.Atoi(s)
	}
	cfg, err := pgxpool.ParseConfig(dsn)
	if err != nil {
		log.Fatal(err)
	}
	cfg.MaxConns = int32(pool)
	cfg.MinConns = int32(pool)
	db, err = pgxpool.NewWithConfig(context.Background(), cfg)
	if err != nil {
		log.Fatal(err)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("GET /plaintext", plaintext)
	mux.HandleFunc("GET /json", jsonHandler)
	mux.HandleFunc("GET /db", single)
	mux.HandleFunc("GET /queries", queries)
	mux.HandleFunc("GET /updates", updates)
	mux.HandleFunc("GET /fortunes", fortunes)
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Server", "go")
		mux.ServeHTTP(w, r)
	})

	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	log.Println("listening on " + port)
	log.Fatal(http.ListenAndServe("0.0.0.0:"+port, handler))
}
