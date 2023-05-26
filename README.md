# ⚠️ In development, not stable ⚠️
many basic features are still lacking, notably:
- delay is not accounted for (goal for v1.0)
- speed is not sycronized (goal v1.0)
- headless server mode (goal for v1.0)
- full android support (future) - will require modifications to mpv-android, or a custom app using libmpv
- windows support (maybe?)

## Voyeurs 🎥
Introducing __voyeurs__ - Unleash the Power of Shared Movie Magic!

Get ready to revolutionize your movie nights 🍿 like never before with __voyeurs__, the sensational open-source CLI tool made in blazingly fast 🚀 and idiomatic Rust 🦀 that effortlessly syncs the playback of __MPV__ videos over any network 🌐. Say goodbye to the endless coordination struggles and welcome hassle-free entertainment sessions with your friends!

### Installation

```
git clone https://github.com/nik012003/voyeurs
cd voyeurs
cargo install --path .
```

### Sample usage
On the server:
```
voyeurs -s 0.0.0.0:8998 -- https://www.youtube.com/watch\?v\=dQw4w9WgXcQ
```
On the clients:
```
voyeurs address.of.server:8998 -a
```