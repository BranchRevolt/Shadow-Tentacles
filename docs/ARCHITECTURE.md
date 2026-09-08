# How it is put together

One process. A Rust core does the work, a system webview shows it, and there is
no server, no daemon and no npm. The window talks to the core through Tauri
commands and gets structured answers back.

## The code

| Path | What lives there |
|---|---|
| `src/pipeline.rs` | The two long jobs: collect, and summarize |
| `src/news/sources/` | Ways into a source: `rss`, `json_api`, `sitemap` |
| `src/news/` | Fetching, extracting, judging and deduplicating articles |
| `src/llm/` | The local engine, remote services, chunking, prompts, languages |
| `src/tts/` | Voices, phonemes, stress, normalization, playback |
| `src/models/` | The catalog, the downloader, hardware detection |
| `src/store.rs` | SQLite: schema, migrations, reads for the feed |
| `src/config.rs` | `sources.toml`: sources, settings, migrations |
| `src/app.rs` | Tauri commands, and the messages the window renders |
| `src/cli.rs` | The same work without a window |
| `ui/` | The window: `index.html`, `app.css`, `app.js`, `i18n.js` |

## The path of an article

```
source  ->  candidate  ->  page  ->  text  ->  verdict  ->  article  ->  summary  ->  card  ->  audio
```

1. **Ask the source.** Every enabled source is asked what it has in the date
   range, and the answers become candidates: an address, a date if one is known,
   and a headline if one is known.
2. **Filter before fetching.** Keywords are applied to the headline, or to the
   words in the address when there is no headline. A sitemap can hand over
   thousands of lines, and deciding there is what keeps the run from reading
   thousands of pages.
3. **Fetch and extract.** The page is fetched and the article is pulled out of
   it with `dom_smoothie`, or with the CSS selectors the source declares.
4. **Judge.** `news::quality::judge` rejects what is not an article: too short,
   too many links for its length, the wrong language for the source, a consent
   page, a redirect stub. Every rejection carries a short reason, and the reason
   reaches the screen.
5. **Deduplicate.** Addresses are canonicalized, and bodies are compared by
   simhash within a Hamming distance, so the same story from two addresses
   becomes one cluster.
6. **Store.** What survives is written to the database and waits to be
   summarized.
7. **Summarize.** The model reads the article and writes a short brief in the
   output language, with tags and a relevance score.
8. **Speak, if asked.** A summary becomes audio on demand, and the player reads
   one card after another.

## Three ways into a source, used together

A single source is reached in as many ways as it supports, and the results are
merged rather than chosen between.

| Reach | Where it comes from | What it gives |
|---|---|---|
| `Feed` | `url` in the source | About thirty items from the last day or two, with the summary the publisher wrote |
| `Titles` | A news sitemap, found through `robots.txt` | Everything published in the last 48 hours, with dates and headlines |
| `Addresses` | An archive sitemap | Everything published, with dates but no headlines |
| Search | `search_url` with `{query}` | Whatever the publisher's own search returns |

The feed keeps its place first because it carries text nobody else provides.
Sitemaps are what turn a keyword into a search instead of a filter over
leftovers: measured on the BBC, the World feed offers thirty items where the news
sitemaps offer more than a thousand over the same two days.

Discovery is automatic. `robots.txt` is read for `Sitemap:` lines, sitemap
indexes are opened to find the lists inside them, and `<lastmod>` or a month in
the address decides which lists are worth opening at all. Budgets keep a run
finite: at most 12 lists, 40 fetches and 300 candidates per source. When a
source is cut short the run says so and gives the number, rather than handing
back a short list that looks like a quiet week.

`sitemap_url` in a source overrides discovery. An empty `sitemap_url` is a
statement rather than a blank: it says this publisher has none, and stops the
program looking on every run.

## What a keyword means

Keywords are matched on stems, not on strings, so `санкции` finds `санкций`.
Written as a list where a leading `-` excludes, they group as OR inside a group
and AND between groups. The window shows the same list as two fields, included
and excluded, because punctuation that changes meaning is a thing to remember
and two boxes are a thing to see.

A headline is not the article. A word can be missed where the sitemap gives only
addresses, and even a headline can hide the subject: measured over two days of
Guardian headlines, `Gaza` appeared in none of 349 headlines while six articles
were about it. This is the honest cost of deciding before fetching, and it is
why the feed is still read in full and why the run reports what it dropped.

## Every run asks the whole question again

A run is complete for the source it asks, whatever it asked last time. Changing
a keyword does not leave yesterday's answer in place, and nothing about the
local database narrows what is fetched. The database is a record of what has
been seen, not a filter on what may be seen.

## The database

SQLite in WAL mode, schema version 7, migrated by `user_version`.

| Table | Holds |
|---|---|
| `articles` | Address, title, text, date, source, cluster, status |
| `articles_fts` | An FTS5 index over titles and bodies, kept by triggers |
| `summaries` | One brief per article, language and prompt version |
| `audio` | Recordings made from summaries |
| `boilerplate` | Fragment hashes, so a site's furniture is stripped once |
| `refused` | Articles a gate turned down, with the reason |
| `source_state` | When each source was last reached |

The feed reads through a `Selection`: sources, a date window, keywords, and the
narrowing typed into the search box over the feed. Paging and counting use the
same `Selection`, which is what lets the numbers on screen agree with each
other.

## Summarizing

Two modes. `Direct` writes the brief in the target language in one pass.
`TwoPass` summarizes in the article's own language first and then translates the
short result, which costs one extra generation over a few hundred tokens instead
of over the whole article.

A run goes batch after batch to the end. The counter is global, so 300 articles
count from 0 to 300 and not from 0 to 30 six times over, and each finished batch
returns its cards to the feed before the next one starts. A batch that resolves
nothing stops the run, which is what keeps an exhausted account or a failing
service from looping.

The model is either local or a service, and choosing it is the whole of the
choice. A local GGUF runs through llama.cpp on the GPU. A service is any address
that speaks the OpenAI chat completions shape, and the screen that offers it says
plainly that the full text of every article is sent there.

## Speaking

A summary is normalized, stressed, turned into phonemes by espeak-ng and spoken
by a Piper voice, all in the process. espeak-ng's phoneme data is packed into the
binary by `build.rs` and written to the data directory the first time a voice
speaks, because the path the library compiles in points at the machine that built
it.

The alternative is Microsoft Edge's reading service, which sounds better and
sends the text to Microsoft, and the screen that offers it says so.

Playback lives in Rust. The window never holds the audio, because the element
that would play it depends on a media plugin a desktop need not have. Rust plays
one recording and knows nothing of what follows it; the queue, the bar and the
step to the next article belong to the window.

## The boundary between Rust and the window

Rust never writes a sentence. It names one: a `Msg` is a code and the values
that go in it, and `ui/i18n.js` turns that into text in whichever of the seven
languages the window is speaking. This is what lets a count agree with its noun,
which four Russian plural forms and one Chinese form make impossible from the
other side.

The page itself is the English dictionary. Every translatable node carries a
`data-i18n` key, and the text written inline is harvested on load, so English
cannot drift out of step with the markup.

Errors follow the same rule. `AppError::Told` carries a code, a message and an
argument, and `is_settled()` marks the refusals that should stop a run rather
than be retried.

## Cancellation

Long jobs take a `Cancel` and check it between steps, so closing the window or
pressing stop ends the run at the next boundary instead of at the end. Shutdown
handles SIGINT and SIGTERM, which is why the `termination` feature of `ctrlc` is
not optional: without it the process dies without unloading the model or
checkpointing the database.

## Where to change what

| To change | Go to |
|---|---|
| How a source is reached | `src/news/sources/` |
| What counts as an article | `src/news/quality.rs` |
| What a summary looks like | `src/llm/summarize.rs` |
| What the feed shows | `src/store.rs` and `ui/app.js` |
| The words on screen | `ui/index.html` for English, `ui/i18n.js` for the rest |
| The shipped source list | `resources/sources.default.toml` |

## See also

- [Building](BUILDING.md), for the toolchain and the platform notes
- [README](../README.md), for using the program
