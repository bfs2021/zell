// TechEmpower Framework Benchmarks scenarios on ASP.NET Core minimal APIs +
// Npgsql (the same service as ../zell/main.zl): /plaintext, /json, /db,
// /queries?queries=N, /fortunes, /updates?queries=N.
// Env: PORT (8080), DB_DSN, DB_POOL (64).
using System.Net;
using System.Text;
using Npgsql;

const string selectWorld = "select id, randomnumber from world where id = $1";
const string updateWorld = "update world set randomnumber = $1 where id = $2";

var pool = int.TryParse(Environment.GetEnvironmentVariable("DB_POOL"), out var p) ? p : 64;
var dsn = Environment.GetEnvironmentVariable("DB_DSN") ??
    "Host=127.0.0.1;Port=5432;Database=hello_world;Username=benchmarkdbuser;Password=benchmarkdbpass;SSL Mode=Disable";
var db = NpgsqlDataSource.Create($"{dsn};Minimum Pool Size={pool};Maximum Pool Size={pool};No Reset On Close=true;Max Auto Prepare=16");

var builder = WebApplication.CreateSlimBuilder(args);
builder.Logging.ClearProviders();
var port = Environment.GetEnvironmentVariable("PORT") ?? "8080";
builder.WebHost.UseUrls($"http://0.0.0.0:{port}");
builder.WebHost.ConfigureKestrel(o => o.AddServerHeader = false);
builder.Services.ConfigureHttpJsonOptions(o => o.SerializerOptions.TypeInfoResolverChain.Insert(0, AppJson.Default));
var app = builder.Build();

app.Use((ctx, next) => { ctx.Response.Headers.Server = "dotnet"; return next(ctx); });

static int RandomId() => Random.Shared.Next(1, 10001);

static int QueryCount(HttpContext ctx)
{
    if (!int.TryParse(ctx.Request.Query["queries"], out var n) || n < 1) return 1;
    return n > 500 ? 500 : n;
}

async Task<World> LoadWorld()
{
    await using var cmd = db.CreateCommand(selectWorld);
    cmd.Parameters.Add(new NpgsqlParameter<int> { TypedValue = RandomId() });
    await using var r = await cmd.ExecuteReaderAsync();
    await r.ReadAsync();
    return new World(r.GetInt32(0), r.GetInt32(1));
}

app.MapGet("/plaintext", () => Results.Text("Hello, World!", "text/plain; charset=utf-8"));
app.MapGet("/json", () => new Msg("Hello, World!"));
app.MapGet("/db", async () => await LoadWorld());
app.MapGet("/queries", async (HttpContext ctx) =>
{
    var n = QueryCount(ctx);
    var ws = new World[n];
    for (var i = 0; i < n; i++) ws[i] = await LoadWorld();
    return ws;
});
app.MapGet("/updates", async (HttpContext ctx) =>
{
    var n = QueryCount(ctx);
    var ws = new World[n];
    for (var i = 0; i < n; i++) ws[i] = (await LoadWorld()) with { RandomNumber = RandomId() };
    foreach (var w in ws)
    {
        await using var cmd = db.CreateCommand(updateWorld);
        cmd.Parameters.Add(new NpgsqlParameter<int> { TypedValue = w.RandomNumber });
        cmd.Parameters.Add(new NpgsqlParameter<int> { TypedValue = w.Id });
        await cmd.ExecuteNonQueryAsync();
    }
    return ws;
});
app.MapGet("/fortunes", async () =>
{
    var fs = new List<Fortune>(16);
    await using (var cmd = db.CreateCommand("select id, message from fortune"))
    await using (var r = await cmd.ExecuteReaderAsync())
        while (await r.ReadAsync()) fs.Add(new Fortune(r.GetInt32(0), r.GetString(1)));
    fs.Add(new Fortune(0, "Additional fortune added at request time."));
    fs.Sort((a, b) => string.CompareOrdinal(a.Message, b.Message));
    var b = new StringBuilder("<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>");
    foreach (var f in fs)
        b.Append("<tr><td>").Append(f.Id).Append("</td><td>").Append(WebUtility.HtmlEncode(f.Message)).Append("</td></tr>");
    b.Append("</table></body></html>");
    return Results.Text(b.ToString(), "text/html; charset=utf-8");
});

Console.Error.WriteLine($"listening on {port}");
app.Run();

record Msg([property: System.Text.Json.Serialization.JsonPropertyName("message")] string Text);
record World(
    [property: System.Text.Json.Serialization.JsonPropertyName("id")] int Id,
    [property: System.Text.Json.Serialization.JsonPropertyName("randomNumber")] int RandomNumber);
record Fortune(int Id, string Message);

[System.Text.Json.Serialization.JsonSerializable(typeof(Msg))]
[System.Text.Json.Serialization.JsonSerializable(typeof(World))]
[System.Text.Json.Serialization.JsonSerializable(typeof(World[]))]
partial class AppJson : System.Text.Json.Serialization.JsonSerializerContext { }
