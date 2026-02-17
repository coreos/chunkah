# Contributing to chunkah

## Getting started

chunkah is written in Rust so you'll need a Rust toolchain set up. It also uses
[just](https://github.com/casey/just) instead of the more common `make`.

To build chunkah and run unit tests:

```bash
just check
```

Your iteration loop as you're hacking on chunkah will likely be mostly this in
early development.

There is also `just checkall` which runs additional checks/lints. Run this
before submitting a PR.

## e2e tests

To run the e2e tests, you first need to build the chunkah image locally. This
can be done using

```bash
just buildimg --no-chunk
```

(The `--no-chunk` option skips the self-chunking step for faster builds.)

And then you can run e2e tests with

```bash
just test
```

It also supports running just some tests (e.g. `just test reproducible`).

## Support tooling

There's also various development tools available to make it easier to debug/test
code. Two are documented here, but definitely look at all the scripts in the
`tools/` directory.

### just split

Helper to split an image using the `Containerfile.splitter` flow. Example usage:

```bash
just split quay.io/fedora/fedora -v
```

### just diff

Helper to diff between two images, usually the original image and the result of
passing it through chunkah. Example usage:

```bash
just diff quay.io/fedora/fedora-minimal localhost/fedora-minimal
```

## AI-assisted contributions

This project heavily uses AI for development. This section sets clear
expectations on using AI for contributions. The guidance below is inspired by
Oxide's [RFD 576] on using LLMs (the whole doc is definitely worth a read!).

### You own what you submit

However the code was produced, the contributor bears full responsibility for
it. AI is a tool; the human using it is accountable for the result. **This means
you MUST understand every line of code you submit** and be able to explain the
design decisions behind it.

### Self-review is mandatory

**DO NOT request reviews on AI-generated code you did not yourself review
first**. Check for correctness and clarity. If you wouldn't be comfortable
defending a line of code in review, don't submit it. Failure to do this
self-review and catching obvious AI slop during the review process wastes time
and erodes trust.

### Don't re-generate in response to review

You can use AI to help address review feedback, but it must be intentional and
targeted to the review areas. **DO NOT regenerate the whole contribution**. The
interdiff (the diff between the last reviewed patches and your addressing of
them) must be in line with what reviewers expect given the feedback they gave.

### Disclose AI usage

Disclosure of AI usage is recommended but not mandatory. The best way to
disclose it is via an `Assisted-by:` trailer in your commit messages to indicate
which AI tool was used. For example:

```text
Assisted-by: Claude Opus 4.6
```

This isn't about shaming or gatekeeping (many/most commits in this repo are
AI-assisted!). It's about providing useful context to make review easier, and
to help us learn collectively about where AI assistance works well and where
it doesn't.

### The AGENTS.md file

The [AGENTS.md](AGENTS.md) file provides detailed guidance to AI coding agents
working in this repository, including project architecture, coding
conventions, and build commands. If you use an AI coding agent, make sure it
has access to this file.

[RFD 576]: https://rfd.shared.oxide.computer/rfd/0576
