# Searching this project's memory with qmd

This directory is the project's memory, and qmd — an on-device search engine
for markdown — is installed in every sandbox and indexes it. Reach for it
before grepping when you want what earlier tasks learned.

Register the directory and build the index once in this sandbox:

```sh
qmd collection add ~/.altitude/memory --name memory
qmd context add qmd://memory "This project's memory: what earlier tasks learned"
qmd embed
```

Then search:

```sh
qmd search "the command that works" -c memory    # keywords (BM25)
qmd vsearch "how deployments go here" -c memory  # meaning, not words
qmd query "why the build is slow" -c memory      # hybrid + reranking
qmd get "notes/build.md" --full                  # one file in full
```

`qmd search` and `qmd query` take `--json`, `--files` and `--min-score` for
feeding results back to yourself. Re-run `qmd embed` after you write a file so
the index includes it. The index lives outside this folder and is cheap to
rebuild: the markdown files are the truth, never the index.

Altitude refreshes this file during provisioning; keep your own notes in
markdown files of your own beside it.
