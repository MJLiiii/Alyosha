# Photos, voice, video, and files

You can send Alyosha what you would send a person in a chat: photos, voice messages, videos, GIFs, stickers, files, locations, and contacts. Alyosha can send files back: a chart it drew, a PDF it filled in, a video it cut. This works the same in Telegram and QQ, as far as each app allows. [Chat channels](Channels.md) · [Telegram bridge](TelegramBridge.md) · [QQ bridge](QQBridge.md) · [Architecture overview](README.md)

Decided on 2026-11-27 and written the same day, after step 10 of the build order. The code passes its scripted checks under `cargo test`, including the media table against a throwaway PostgreSQL server and FFmpeg on a sample clip when FFmpeg is on `PATH`. [Where to start implementing](README.md#where-to-start-implementing)

## What the model sees

The model reads text and looks at images. Everything you send becomes one or the other, so it works with every model route and does not depend on one provider's audio or video support. Only Gemini reads video natively and Claude does not read audio, so turning them into text and pictures keeps a change of model from breaking them. [LLM Client / Gateway](LLMClientGateway.md)

| You send | The model gets |
| --- | --- |
| A photo, an image file, a static sticker | The image |
| An animated sticker or a video sticker | Its still thumbnail, and the sticker's emoji |
| A GIF, a video, a round video message | Up to eight frames spread over its length, and a transcript of its sound |
| A voice message or an audio file | A transcript, such as `[voice 0:42] Can you move tomorrow's call to 3?` |
| A PDF or any other file | A line with the path where it was saved, such as `[file: inbox/2026-11-27-1430-invoice.pdf, 182 KB]`. The model reads it with its usual tools, for example `pdftotext` in `run_command`. |
| A location, a contact, a poll, a dice roll | A line of text, such as `[location: 52.3731, 4.8922, Dam Square]` |
| An album of several photos or videos | One message holding all of them |

A caption is the message's text. A message that replies to someone else's message brings that message's text and attachments along, so "what does this say?" in reply to a photo works. A reply to one of Alyosha's own messages brings nothing extra, because the model already has it.

OpenAI transcribes speech, through GPROXY with your OpenAI API key, in whatever language you speak. [Transcription](#transcription) If transcription fails or is not set up, the model gets a line saying so instead of a transcript, and the turn goes on, so it can tell you.

## Where things are kept

- **Images are kept in the session database**, with the conversation, so they survive restarts and are in the daily dump. Each is shrunk so its longer side is at most 2048 pixels; models shrink larger images anyway. The same image sent twice is stored once. Video frames are kept the same way. [Durable sessions](SessionStore.md)
- **Every attachment is also saved as a file** in the session's working folder, under `inbox/`: `work/owner/inbox/` for your private chats and `work/groups/<name>/inbox/` for a trusted group. Commands cannot reach the database, so this is how the model works on a file, cuts a video, or sends a photo on. The original is saved, not the shrunk copy. The working folders are not backed up, and nothing in `inbox/` is ever deleted by Alyosha. [Where things live](Deployment.md#where-things-live)
- **Voice and video are not kept in the database.** Only their transcript and frames are, in the conversation.

## Guests

Guest sessions answer people in groups you have not listed as trusted. [Group chats](Channels.md#group-chats) They get images, stickers, GIFs, video frames, transcripts, and the text lines, but not files: a guest session has no working folder, so a file becomes a line such as `[file report.pdf: files are not read in this chat]`. Nothing a guest sends is saved as a file. Each image and transcript uses your model and OpenAI quota, like every guest message. Guests cannot send files back.

## Sending files

The `send_file` tool sends a file from the working folder to the chat the conversation is in, with an optional caption. The bridge picks how it goes out from the file's type: a photo shows as a photo, a video plays in the chat, an Opus voice recording plays as a voice message, and anything else arrives as a file. Files go out after the turn's reply text, in the order the model sent them. Files sent from a scheduled task or a finished job go where its reply goes. [Built-in tools](reference/BuiltInTools.md#send_file)

`send_file` is in your private sessions and trusted groups, not in guest sessions. A file must be in the working folder and at most 50 MB, the most Telegram lets a bot send.

## Images the tools return

The model also sees images its tools return, not only the ones you send: a chart it drew and opens with `read_file`, a photo in `inbox/`, an image an MCP server returns, and a screenshot of a web page. "Does this chart look right?", "what is in the photo I sent yesterday?", or "what does the graph on this page show?" then work. [Built-in tools](reference/BuiltInTools.md#read_file) · [MCP results](MCP.md#handle-one-call) · [Browser screenshots](Browser.md#a-small-set-of-tools)

- **Shrunk and kept like yours.** A returned image goes through the same steps as one you send: at most 2048 pixels on the longer side, stored once in the session database, in the conversation from then on. [Images](#images)
- **Four per result.** A tool result shows at most four images. Further ones become a line saying how many were left out.
- **Where the route cannot.** The codex and OpenAI routes and Claude take images in tool results. Chat Completions, such as DeepSeek, takes only text there, and Gemini's support is unchecked, so on those routes each image becomes `[image not shown: this route cannot return images from tools]`. With `[model] images = false`, it becomes `[image not shown: this model cannot see images]`, as for images you send.
- **Text only in Code Mode.** A script's `read_file` of an image is an error. Scripts filter text. [Read-only Code Mode](ToolExecutor.md#read-only-code-mode)

## Limits

| Limit | Why |
| --- | --- |
| 20 MB per attachment from Telegram | The Bot API downloads nothing larger. A larger file reaches the model as a line saying it was too large, so it can tell you. |
| 25 MB per attachment from QQ | A bound of Alyosha's own, until QQ's own limit is known. |
| 25 MB of sound per transcription | OpenAI's limit. Alyosha sends the sound as compressed Opus, so this is about two hours. |
| Eight frames per video or GIF | Each frame costs as much as an image, about 1,600 tokens. |
| Four images per tool result, 20 MB per image file `read_file` opens | The same cost per image. A larger image file is a tool error. |
| Ten attachments per message | Telegram's album limit. Further attachments become a line saying they were left out. |
| 50 MB per file sent, 5 MB on QQ | Telegram's limit for bots, and the most QQ takes in one upload request. QQ's chunked upload is deferred. |

## Deferred

- Images in tool results on Chat Completions and Gemini, for example as a user message after the results.
- PDFs as page images, and audio that MCP servers return.
- Images in Code Mode.
- Voice replies spoken by Alyosha, generated images, and live voice calls.
- Telegram files over 20 MB, which need a Bot API server of your own.
- Sending the model video or audio natively on routes that take it.
- QQ's chunked upload, for files over 5 MB.
- A cache of image bytes in memory, instead of reading them from the database for each request.

## Implementation

Build this after step 10. It changes `alyosha acp`, both bridges, the history entry, the session store, the Context Manager, the LLM Client, and the container, and adds one built-in tool. Each of those pages has a short section that links back here. [Architecture overview](README.md)

### From the bridge to `alyosha acp`

`alyosha acp` answers `initialize` with `promptCapabilities` `image`, `audio`, and `embeddedContext` all `true`. [Initialization](Channels.md#initialization) A bridge downloads each attachment and sends it in the `session/prompt` as a standard ACP content block, in the order the chat showed them, after the message's text:

| Attachment | ACP block |
| --- | --- |
| Image | `{"type": "image", "mimeType": ..., "data": <base64>}` |
| Voice or audio | `{"type": "audio", "mimeType": ..., "data": <base64>}` |
| Video, GIF, or any other file | `{"type": "resource", "resource": {"uri": "attachment:///<file name>", "mimeType": ..., "blob": <base64>}}` |
| Location, contact, poll, dice, a file too large to download | A `text` block with the line from [What the model sees](#what-the-model-sees) |

An image or audio block may carry `_meta: {"alyosha": {"name": "<file name>"}}`, which names the file in `inbox/`. The bridges share the code that builds these blocks and the text lines in `alyosha-bridge`'s `attachments` module. A bridge never decodes or converts media itself; that happens in one place, `alyosha acp`.

Alyosha refuses a prompt with more than ten attachment blocks, or one block over 25 MB after decoding, with an invalid-params error. The bridges already enforce both, so this only guards against a broken bridge.

### Preparing a message

`UserMessage` gains `attachments: Vec<Attachment>`, each holding the bytes, the MIME type, and a file name. The queue holds them as they came. [Agent State](AgentState.md#session-fields)

When the message's turn starts, before `run_turn` appends the `User` entry, prepare each attachment in order. While it runs, show a status line, **Reading attachments…**, as a pseudo tool call. Stop during preparation ends the turn `Stopped` and the message never enters history. [Status notices](Channels.md#prompts)

1. **Save the file.** In a session with a working folder, write the original to `inbox/<YYYY-MM-DD>-<HHMM>-<name>` through `replace_file`, creating `inbox/` if missing. The date and time are the message's, in the configured timezone. `<name>` is the attachment's file name with any `/` and control characters removed, or `photo.jpg`, `voice.ogg`, `video.mp4` by type when there is none. If the name is taken, add `-2`, `-3`, and so on before the extension. In a guest session, save nothing. [Atomic file replacement](EnvironmentInterface.md#atomic-file-replacement)
2. **Turn it into parts** by its MIME type, sniffing the bytes when the type is missing or `application/octet-stream`:

   | Type | Parts |
   | --- | --- |
   | A still image | One `Image` part |
   | An animated GIF or WebP | Up to eight frames, decoded in Rust, each an `Image` part |
   | `audio/*` | A `Text` part with the transcript |
   | `video/*` | The frames as `Image` parts, then a `Text` part with the transcript if it has sound |
   | Anything else | No parts beyond the line in step 3 |

3. **Add a line naming it**, before its parts: `[photo: inbox/…]`, `[voice 0:42: inbox/…]`, `[video 1:15: inbox/…]`, `[file: inbox/…, 182 KB]`, with the duration as `M:SS`. In a guest session the line has no path, and a file becomes `[file <name>: files are not read in this chat]`.

A step that fails adds what failed to the attachment's line, such as `[voice 0:42: inbox/…, could not transcribe: rate limit]`, and preparation goes on. The `User` entry's parts are, in order: the clock line and the message's text as one `Text` part, then each attachment's line and parts.

### Images

Decode images in the `alyosha acp` process with the `image` crate, whose decoders are written in Rust. Refuse an image over 50 megapixels or one that needs more than 512 MB to decode, as a failed step. Apply the EXIF orientation, shrink it so the longer side is at most 2048 pixels, or 1024 for a video frame, and encode it as JPEG at quality 85, or as PNG if it has transparency. Store the result in the media table and keep the reference in the part.

```rust
enum UserPart {
    Text(String),
    Image(MediaRef),
}

struct MediaRef {
    sha256: String,     // hex, of the stored bytes
    mime: String,       // image/jpeg or image/png
    width: u32,
    height: u32,
}
```

With `[model] images = false`, store nothing and use a `Text` part instead: `[image not shown: this model cannot see images]`. [Configuration](reference/Configuration.md#model)

### Video and sound in a media unit

Decoding video means running FFmpeg on files from strangers. It runs in a unit of its own, not in `alyosha acp`, whose user can read your secrets.

1. Create `/run/alyosha-run/<process ID>-<counter>/`, write the attachment there as `input`, and start `alyosha-run-media@<process ID>-<counter>.service`. The name matches the command units' polkit rule, slice, and loopback rule, and their cleanup of leftovers. [Command units](Deployment.md#command-units)
2. The unit runs `alyosha media-exec %i`, which runs FFmpeg with fixed arguments only: read the duration, write up to eight frames spread evenly over it, at least one second apart, as `frame-<n>.jpg`, and write the sound, if any, as mono Opus at 24 kbit/s in `audio.ogg`. It writes `result.json` with the duration, the frame files, and whether there was sound.
3. Wait for the unit to finish, with a limit of five minutes. Read the results, remove the folder, and treat a failed or timed-out unit as a failed step.

Voice messages already in Ogg Opus, as Telegram sends them, go to transcription without the unit, with their length read from the last Ogg page's granule position. Other audio goes through the unit for its sound only.

### Transcription

Send the sound to OpenAI's transcription API through GPROXY: `POST <gateway url>/<transcribe_provider>/v1/audio/transcriptions`, with the gateway key, as `multipart/form-data` with `file` and `model`, without a language, so OpenAI detects it. Read `text` from the JSON reply. Use the same 10-second connect and 5-minute response limits as model requests. Sound over 25 MB is a failed step. [Model requests](LLMClientGateway.md#v1-caller-contract)

`[media] transcribe_provider` names a GPROXY provider on the `openai` channel, and `transcribe_model` the model, `gpt-transcribe` by default. Without `transcribe_provider`, the step fails with `transcription is not set up`. The API key has a login of its own, `alyosha gateway login transcription`, separate from the model's `alyosha gateway login`, so setting or replacing the key never asks you to sign in to ChatGPT again, and signing in again never asks for the key. It creates the provider when missing, reads the key without echo, and refuses a key with anything but letters, digits, `-`, and `_`, which means more than the key was pasted, without showing the key. GPROXY keeps the key encrypted in its own folder in the container, so neither the settings nor your NixOS configuration ever hold it. A transcription OpenAI answers with 401 or 403 says the key was refused and names the command. [Gateway login](LLMClientGateway.md#run-gproxy-as-a-service) · [Configuration](reference/Configuration.md#media)

GPROXY forwards the multipart body: on ms02 on 2026-11-27, a voice message reached OpenAI through it, which answered 401 for a key pasted with extra characters. No direct connection to OpenAI is needed.

### The media table

Migration `003_media.sql`:

```sql
create table media (
    sha256      text primary key,
    mime        text not null,
    bytes       bytea not null,
    created_at  timestamptz not null default now()
);
```

`SessionStore` gains `put_media(mime, bytes) -> MediaRef`, which inserts with `on conflict do nothing`, and `get_media(sha256) -> Option<(mime, bytes)>`. `MemoryStore` keeps them in a map. Rows are never deleted in v1, like sessions. [Durable sessions: schema](SessionStore.md#schema)

### Sending files out

`send_file` takes `path` and an optional `caption` of at most 1,024 characters. [Tool contract](reference/BuiltInTools.md#send_file)

1. Open the file with `open_file(path, Read)`, so the usual workspace checks refuse symlinks out of it. Refuse anything that is not a regular file in the workspace, and anything over 50 MB.
2. Copy it, through that handle, into the session's own folder, as `outbox/<n>-<name>` in `/var/cache/alyosha/sessions/<process ID>-<random>/`. The copy is what the bridge sends, so a file the model changes or swaps for a symlink afterwards cannot change what goes out. [Session logs](EnvironmentInterface.md#session-logs)
3. Send an `agent_message_chunk` whose content is a `resource_link`: `uri` the copy's `file://` path, `name` the original name, `mimeType` from the extension, `size`, and the caption as `description`.
4. Return `{"status": "queued", "name": ..., "bytes": ...}`.

The bridge collects the links of a prompt in order and sends the files after the reply. It deletes each copy once sent or given up on. The session folder is removed when the session ends, as now.

### Images in tool results

A handler returns raw images next to its text, and the Agent Loop prepares them before it records the result, as it prepares attachments before a `User` entry. [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

```rust
struct ToolOutput { text, notice, kind, images: Vec<RawImage> }   // images is new
struct RawImage { mime: String, bytes: Vec<u8> }

struct ToolResultEntry {
    call_id: CallId,
    outcome: ToolOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<MediaRef>,      // new; an entry without images is stored as before
}
```

- **Preparing.** For each of the first four images, run the steps of [Images](#images): decode, refuse over 50 megapixels or 512 MB, orient, shrink to 2048 pixels, encode as JPEG or PNG, and `put_media`. An image that fails adds the line `[image could not be read: <reason>]` to the text instead, and the call still succeeds. Further images add `[N more images not shown]`. Without media services, as in the scripted tests' minimal sessions, or with `[model] images = false`, add the matching `image not shown` line and store nothing. A tool error carries no images.
- **`read_file`.** Before reading text, look at the file's first bytes. PNG, JPEG, GIF, and WebP signatures make the result an image: the text `<path>: <mime>, <width>×<height>, <size>` and the file as one `RawImage`, read through the handle `open_file` returned. A GIF shows its first frame. Over 20 MB is a tool error, **<path> is an image over 20 MB.** `offset` and `max_bytes` are ignored for an image. In Code Mode an image is the tool error **<path> is an image. Code Mode reads text only.** [Built-in tool contracts](reference/BuiltInTools.md#read_file)
- **MCP.** Each `image` content block, base64 `data` with a `mimeType`, becomes a `RawImage`; one whose data does not decode as base64 becomes the `could not be read` line. `audio` keeps its placeholder. [Handle one call](MCP.md#handle-one-call)
- **Encoding.** An entry's images follow its text, in order. Each route's native form is in [Images in requests](LLMClientGateway.md#images-in-requests). The Agent Loop loads their bytes into the request's `media` map as it does for `User` entries.
- **Limits and summaries.** The result limits apply to the text only. The Context Manager counts each image as 1,600 tokens. A summary transcript shows each as `[image]` after the result's excerpt, and `session/load` replays nothing for them. Recall searches the text only. [Token budgets](ContextManager.md#v1-token-budgets) · [Recall](Recall.md)

### Context and compaction

- **Requests.** Before each model request, the Agent Loop loads the bytes of every image in it with `get_media` into the request's `media` map, base64-encoded. A missing row fails the turn with an error naming the hash. The LLM Client sends them inline, in their place among the text parts. [Image parts](LLMClientGateway.md#images-in-requests)
- **Estimates.** The Context Manager counts each image as 1,600 tokens. [Token budgets](ContextManager.md#v1-token-budgets)
- **Summaries.** The summary transcript shows each image as `[image]`. The line in front of it, such as `[photo: inbox/…]`, already tells the summary where the file is.
- **Replay.** `session/load` replays a `User` entry's `Text` parts and shows each image as `[image]`. [ACP replay](SessionStore.md#acp)

### Verification

These run under `cargo test` with the scripted model, a fake transcription server, and a fake media unit that returns fixed frames. [Turn fixture](AgentLoop.md#verification-with-a-scripted-model-and-fake-tools)

- `initialize` reports `image`, `audio`, and `embeddedContext` support.
- Send a prompt with text and a 4000×3000 JPEG. Check that the `User` entry holds the text, the `[photo: inbox/…]` line, and an `Image` part of 2048×1536; that the original is in `inbox/` with the message's date in its name; and that the model request carries the image after the text on each route's fixture.
- Send the same image in two messages. Check one media row and two files in `inbox/`, the second with `-2`.
- Send an image with transparency, then a 60-megapixel image. Check a PNG part, then a failure line and a reply that still runs.
- Send Ogg Opus voice. Check that the fake transcription server receives it without a media unit and the part reads `[voice 0:12: inbox/…]` then the transcript. Make the server fail. Check the failure line and that the turn completes. Remove `transcribe_provider`. Check `transcription is not set up`.
- Send an MP4. Check that a media unit starts with the expected name, that its frames become `Image` parts of at most 1024 pixels followed by the transcript, and that its folder is removed. Make the unit time out. Check the failure line.
- Send an animated GIF. Check that its frames are decoded without a media unit.
- Send a PDF in a guest session. Check the `files are not read in this chat` line and that nothing is written. Send an image there. Check that it is shown.
- Set `[model] images = false`. Check the `image not shown` line and no media row.
- Send eleven attachments. Check that `alyosha acp` refuses the prompt.
- Stop during preparation. Check the `Stopped` outcome and no `User` entry.
- Load a stored v1 `User` entry. Check that it reads as one `Text` part. Resume a session with an image over ACP. Check that the replay shows `[image]`.
- Script a summary over an image. Check that the transcript shows `[image]` and that the estimate counted 1,600 tokens for it.
- Script `read_file` on a 4000×3000 PNG in the workspace. Check the text line, one image of 2048×1536 in the `ToolResult` entry, one media row, and the image in the next request on the Responses and Claude fixtures and the `image not shown` line on the Chat Completions and Gemini fixtures. Read a text file, then a 21 MB JPEG and a PNG from Code Mode. Check the text, then the two tool errors.
- Return a broken image and then five images from a tool. Check three images, the `could not be read` line, and `[2 more images not shown]`, and that the next request carries the three. Set `[model] images = false`. Check the `image not shown` lines and no media rows.
- Store a `ToolResult` entry with images and load it. Check it reads back with its images, and that an entry stored before this change reads with none. Script a summary over it. Check `[image]` in the transcript and 1,600 tokens per image in the estimate.
- Call `send_file` on a workspace file. Check the copy in `outbox/`, the `resource_link` before `end_turn`, and the result. Call it on a symlink to a file outside the workspace, on a folder, and on a 51 MB file. Check the tool errors. Check that a guest session has no `send_file`.

In the container:

- Start a media unit by hand on a sample video. Check the frames and `audio.ogg`, that `curl` to any address fails inside it, and that it cannot read `/var/lib/alyosha-secrets`.
- Run `alyosha gateway login transcription` with a key followed by a space and more text. Check that it is refused without showing the key. Run it with the key alone. Check the test transcription, then send a voice message and check the transcript.

With real bots:

- From Telegram send a photo, an album of three, a voice message, a round video, a GIF, an animated sticker, a PDF, and a location, and ask about each. Check the answers and the files in `work/owner/inbox/`.
- From QQ send a photo, a voice message, a video, and a file. Record what QQ sends for each, then check the answers.
- Ask Alyosha to open a page with a chart and say what it shows. Check that it takes a screenshot and describes the chart. Through GPROXY on ms02 on 2026-11-27, the codex route answered "Red" to a tool result holding a red 2×2 PNG.
- Ask Alyosha to draw a chart and send it, and to send a PDF. Check that Telegram shows a photo and a file, and that QQ shows them within its four replies.

[Architecture overview](README.md)
