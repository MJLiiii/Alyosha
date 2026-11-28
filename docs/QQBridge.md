# Implement the QQ bridge

`alyosha-qq` relays private (C2C) messages between your QQ account and one `alyosha acp` child process, using QQ's official bot API v2. Build it after the Telegram bridge, reusing the shared bridge crate. [Chat channels design](Channels.md#the-qq-bridge) · [Telegram bridge](TelegramBridge.md) · [ACP mode](Channels.md#implementation) · [Architecture overview](README.md)

Facts below come from the [QQ bot documentation](https://bot.q.qq.com/wiki) as read in November 2026. The platform changes its limits often, so check the linked pages again when building.

## Package layout

`alyosha-qq` is a third package in the Cargo workspace, in `qq/`, next to `telegram/`. It depends on the shared bridge crate for the ACP side and calls the QQ API directly with `reqwest` and `tokio-tungstenite`, since it needs only a token endpoint, the WebSocket, and one send endpoint. [Package layout](TelegramBridge.md#package-layout)

## Register the bot

1. Sign in to the [QQ open platform](https://q.qq.com) with a QQ account that has completed real-name verification, and create a bot as an individual.
2. Copy the AppID and AppSecret from the bot's development settings.
3. Choose WebSocket for event delivery.
4. Use the bot from the administrator account, or add your QQ number as a test user under 「开发设置」. Do not publish the bot.

## Configuration

```toml
[qq]
app_id = "102000000"
owner_openid = ""                             # fill in from the log, see below
owner_member_openids = []                     # optional: your member_openid in trusted groups, from the log
reply_chunk = 1500                            # characters per message

[qq.trusted_groups]                           # optional: groups with your powers
friends = "<group_openid>"                    # folder name = group_openid
```

In Nix, this is `services.alyosha.settings.qq`. Load it with `alyosha-config`, which checks every value, and read the AppSecret from the secret `qq-secret`, set with `alyosha secret set qq-secret`. Refuse to start, with a clear error, if the `[qq]` section or the secret is missing. Never log the secret or an access token. [Configuration](reference/Configuration.md#qq) · [Secrets](reference/Configuration.md#secrets)

QQ identifies you by a `user_openid` specific to this bot, not your QQ number. While `owner_openid` is empty, the bridge starts, answers nobody, and logs the `user_openid` of every message. Send the bot a message, copy your ID from the log, and restart.

## Access token

Get a token with `POST https://api.bot.qq.com/app/getAppAccessToken` and body `{"appId": ..., "clientSecret": ...}`. The reply holds `access_token` and `expires_in`, about 7200 seconds. Some replies give `expires_in` as a string, so accept both. A failed request still returns HTTP 200 with a `code` field. Treat any `code` as failure. Request a new token in the last 60 seconds of the old one's life. The old token stays valid for those 60 seconds. [Access token](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/access-token.html)

Send `Authorization: QQBot <access_token>` on every API call to `https://api.bot.qq.com`. API errors carry `err_code`, `message`, and `trace_id`. Log the trace ID.

## WebSocket connection

1. `GET /gateway` returns the WebSocket URL.
2. Connect. The server sends op `10` with `heartbeat_interval` in milliseconds.
3. Send op `2` (Identify) with `token` set to `QQBot <access_token>`, `intents` set to `GROUP_AND_C2C_EVENT` (`1 << 25`), and `shard` set to `[0, 1]`. Wait for the `READY` dispatch and keep its `session_id`.
4. Send op `1` (Heartbeat) every interval, carrying the last sequence number `s` received, or `null` before the first. Expect op `11` in reply.
5. On a dropped connection or op `7` (Reconnect), reconnect and send op `6` (Resume) with the token, `session_id`, and last sequence. On op `9` or close codes `4006` and `4007`, identify again. On `4008`, wait before reconnecting. Close codes `4013`, `4014`, `4914`, and `4915` mean the intent, permission, or bot itself is not allowed. Log them and exit with an error.
6. Log `READY` and `RESUMED` at info level, each new identify after a lost session, and the type of every dispatch the bridge does not handle, with its `group_openid` if it has one. These show whether QQ is sending anything when a mention gets no reply. Never log message content.

[WebSocket events](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/websocket.html) · [Payload and intents](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/payload.html)

## Incoming messages

Handle only `C2C_MESSAGE_CREATE` dispatches. In each, `d.id` is the message ID, `d.author.user_openid` the sender, `d.content` the text, and `d.attachments` any files. QQ may deliver the same message twice. Skip a message ID already seen in the last hour. [C2C message event](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/c2c_message_create.html)

Drop a message, logging the sender's `user_openid`, unless it equals `owner_openid`. Send nothing back.

For a message from the owner, trim the text, then act as the Telegram bridge does: `/new`, `/cancel`, `/compact`, `/job`, and `/tasks` behave the same, `/new <name>` and `/cancel <name>` act on the QQ trusted group under that name, and other text becomes a `session/prompt`. Send `initialize` with `clientInfo.name` set to `alyosha-qq`, and start the task runner for the `qq` channel. Handle `_alyosha/job_finished` as the Telegram bridge does. Scheduled and job replies have no message to answer and follow their own delivery rules. [Delivering on QQ](Schedule.md#deliver-on-qq) A message with attachments becomes a prompt with their blocks after its text. [Attachments](#attachments) · [Incoming messages](TelegramBridge.md#incoming-messages)

## Group chats

Handle `GROUP_AT_MESSAGE_CREATE` dispatches too, which use the intent the bridge already asks for. QQ sends them only for messages that @ the bot and removes the @ from `content`. In each, `d.id` is the message ID, `d.group_openid` the group, `d.author.member_openid` the sender, `d.author.member_nickname` the display name when present, and `d.content` the text. Skip duplicate message IDs as for private messages.

A group where the bot has QQ's "receive all messages" mode on sends every message as `GROUP_MESSAGE_CREATE`, @s included, with the same fields. Treat one as a mention only when `d.mentions` has an entry with `is_you` true, and remove that entry's `<@member_openid>` (and `<@id>`) tokens from `content`, which QQ removes by itself in `GROUP_AT_MESSAGE_CREATE`. Ignore the rest, logging only the group and the number of mentions. If QQ sends both events for one @, the duplicate check on the message ID drops the second. [Group message event, all messages](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_message_create.html) · [Group @ message event](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_at_message_create.html) · [Group chats design](Channels.md#group-chats)

A mention in a group whose `group_openid` is in `trusted_groups` goes to the group's full session, as in the Telegram bridge: `alyosha acp --group` in `/var/lib/alyosha/work/groups/<name>`, with the text and the sender in `_meta` as the prompt, marked `"owner": true` when the sender's `member_openid` is in `owner_member_openids`, approvals sent to the owner's private chat after **In group <name>:**, and finished jobs given to the group's session with the reply sent to the group as a reply to its latest mention, which falls back to an active message once that window has closed. The pool never stops it for being idle. A `/yes`, `/no`, or number from the owner answers the oldest open request, whichever session it came from; `/cancel` cancels only the private session and its requests. A mention whose whole text is `/new`, `/cancel`, `/compact`, or `/job …` is a command for the group's session, from anyone there, handled as in the Telegram bridge, with the key `group:qq:<group_openid>`; the bridge's replies go to the group as replies to that mention. [Trusted groups](TelegramBridge.md#trusted-groups) · [Commands in trusted groups](Channels.md#commands-in-trusted-groups)

Every other group message goes to the group's guest session from the shared pool, the owner's included, as the Telegram bridge does: `initialize` with `alyosha-qq`, the sender `qq-<member_openid>`, and the nickname, or `QQ user` without one. Attachments go along as in a private message, and the guest session decides what it reads. [Guests](Media.md#guests) · [Telegram group chats](TelegramBridge.md#group-chats)

Send the reply with `POST /v2/groups/{group_openid}/messages`, as passive replies to the mention's `msg_id` with `msg_seq` counting from 1, split into at most four parts as for private replies. QQ allows five replies within 60 minutes. If the window has closed, try the same request without `msg_id`. Log a reply that still fails, and drop it. Nothing is kept for later in a group, and there is no typing indicator. Log the `group_openid` and the sender's `member_openid` of every mention at info level, so the owner can copy them into `trusted_groups` and `owner_member_openids`. [Send a group message](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)

## Attachments

Each entry of `d.attachments` has a `content_type`, a `url`, and usually a `filename` and `size`. A voice entry may also carry `voice_wav_url`, a WAV copy of the SILK recording, and `asr_refer_text`, QQ's own transcript. [C2C message event](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/c2c_message_create.html) Build blocks with the shared crate's `attachments` module, as the Telegram bridge does. [Telegram attachments](TelegramBridge.md#attachments) · [From the bridge to `alyosha acp`](Media.md#from-the-bridge-to-alyosha-acp)

| `content_type` | Block |
| --- | --- |
| `image/*`, including `image/gif` | `image` |
| `voice` | `audio` from `voice_wav_url` when the entry has one. Otherwise, if it has `asr_refer_text`, QQ's own transcript as the text `[voice: <text>]`. Otherwise the text `[voice message: QQ sent it in a format Alyosha cannot read]`. |
| `video/*` | `resource` |
| anything else | `resource` with `filename` |

Download each `url`, adding `https:` when it starts with `//`. Skip an entry whose `size` is over 25 MB, or whose download passes 25 MB, and send `[<kind> <file name> too large to download: <size> MB]` in its place. Keep at most ten attachments.

QQ records voice as SILK, which FFmpeg cannot decode, so Alyosha relies on QQ's WAV copy or its transcript. The first check with a real bot records which of them QQ actually sends. Decoding SILK is deferred until it turns out to be needed.

## Sending replies

Each prompt remembers the message ID it came from and a reply counter starting at 1. Send replies with `POST /v2/users/{owner_openid}/messages`, body `msg_type: 0`, `content`, `msg_id` set to the prompt's message ID, and `msg_seq` set to the counter, which increases with every send. The same `msg_id` and `msg_seq` twice fails. [Send a C2C message](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html)

QQ allows four replies per message within 60 minutes:

1. While the turn runs, send the typing indicator every 50 seconds: `msg_type: 6` with `input_notify: {"input_type": 1, "input_second": 60}` and the prompt's `msg_id`. Stop if QQ counts it as a reply. See verification.
2. When the prompt's response arrives, build the reply text as the Telegram bridge does, without the **Working…** message. [Showing progress and replies](TelegramBridge.md#showing-progress-and-replies)
3. Split it into parts of at most `reply_chunk` characters at line breaks. If more than four parts result, keep three and make the fourth the start of the remaining text, cut to fit, followed by **(cut off — ask me to continue)**.
4. Send the parts in order as passive replies.

The model writes Markdown. For a passive reply, first send each part as QQ's native Markdown: `msg_type: 2` with `markdown: {"content": ...}` instead of `content`, split as the Telegram bridge splits Markdown, so code blocks stay closed in each part. If QQ refuses a part for any reason other than the reply window, log the code, send that part and the rest as plain text, and send every later reply as plain text until the bridge restarts. Most bots need QQ's permission for native Markdown, so the first reply after a restart may be refused. Plain text is the reply with the Markdown markers removed, as the shared crate's `markdown` module makes it. Scheduled replies, active messages, approvals, and the bridge's notices are always plain text. [Markdown messages](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/type/markdown.html)

If a send fails with `40034128` (reply limit or window exceeded), `40034005`, or `304103` (message ID expired), send the remaining parts as active messages: the same request without `msg_id`. If that fails too, for example with `40034100` (rate limit) or `40034105` (no permission), keep the unsent text. When the owner's next message arrives, send the kept text first, as that message's replies, before relaying the new message. Log every failed send with its error code.

On `40054007` (message too long), halve the part and retry once, and log it so `reply_chunk` can be lowered.

### Sending files

A prompt's `resource_link` chunks are files from `send_file`. Send them after the reply's text parts, each as one more reply that counts toward the four. [Sending files out](Media.md#sending-files-out)

1. Upload it with `POST /v2/users/{openid}/files`, or `/v2/groups/{group_openid}/files` in a group, with `file_data` as base64, `file_name`, `srv_send_msg: false`, and `file_type` 1 for PNG and JPEG images, 2 for MP4 video, 3 for voice (`audio/ogg`, `audio/mpeg`, `audio/wav`), and 4 for anything else. If QQ answers `850019` (unsupported format) for type 3, upload the file again as type 4. The reply holds `file_info`, which works only in the kind of chat it was uploaded for. `file_data` takes at most 5 MB, so a larger file gets **Could not send <name>: QQ takes files up to 5 MB from Alyosha.** QQ's chunked upload, which takes up to 200 MB, is deferred. [Rich media](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/rich-media.html) · [Upload for a private chat](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_files.post.html) · [Upload for a group](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_files.post.html)
2. Send `msg_type: 7` with `media: {"file_info": ...}` and the link's `description` as `content`: a passive reply while one is left, then an active message. A file that still cannot be sent becomes the line **Could not send <name>:** and the reason, which is delivered like reply text, and kept for your next message if need be. The copy is deleted either way.
3. If QQ refuses the upload otherwise, for example with `850031` (too large) or `40093002` (daily file quota used up), the same line says so.

In a group, files that do not fit in the four replies are dropped with a log line, like any group reply.

## Approvals and questions

On `session/request_permission`, send the request's `title`, a blank line, the `content` text, and then one line telling the owner how to answer:

- For two options: **Reply /yes for <first option> or /no for <second option>.**
- For more: a numbered list of the options, then **Reply with a number.**

Send it as a reply to the prompt's message, by the usual rules, or by the scheduled-reply rules for a scheduled prompt. Splitting counts against the four replies, so cut the body to fit and end it with **(cut off)**. The model formats nothing here: the title and body come from Alyosha. [Sending replies](#sending-replies) · [Delivering on QQ](Schedule.md#deliver-on-qq)

While a request is open, an owner message of exactly `/yes`, `/no`, or a valid number answers it with `selected` and the matching `optionId`. The bridge replies **Answered: <option name>** to that message, and later replies for the same prompt use that message's ID, which starts a fresh count of four. Any other message is a normal prompt. `/cancel` sends `session/cancel` first, then answers open requests with `cancelled`, as the Telegram bridge does. A `failed` update for the call means Alyosha stopped waiting. Reply **Expired — not approved.** to the next owner message.

If the request cannot be delivered by any means, answer it with `cancelled` at once, rather than keeping it for later, so Alyosha treats it as undelivered.

## Failures and shutdown

Follow the Telegram bridge: exit with a nonzero status when `alyosha acp` exits, after telling the owner if a reply is still possible. The restarted bridge loads its sessions under the keys `owner:qq`, `group:qq:<group_openid>`, and `guest:qq:<group_openid>`. [Durable sessions: bridges](SessionStore.md#bridges) On SIGTERM, close the WebSocket, then the child's standard input, wait up to 10 seconds, and kill it. Retry network errors with backoff up to one minute. [Failures and shutdown](TelegramBridge.md#failures-and-shutdown)

## Run it in the container

Add a service next to `alyosha-telegram`, the same apart from its name and `ExecStart = "${alyosha}/bin/alyosha-qq"`. [Run it in the container](TelegramBridge.md#run-it-in-the-container)

## Verification

These run under `cargo test` with a fake QQ API, serving both HTTP and the WebSocket, and a fake ACP agent.

- Start without the `[qq]` section, and without the `qq-secret` secret. Check that each refuses and no secret or token appears in output.
- Start with an empty `owner_openid` and send a message. Check that nothing is relayed or sent back and the sender's `user_openid` is logged.
- Return a token reply with `expires_in` as a string, and one with a `code`. Check that the first works and the second fails clearly.
- Drop the WebSocket after `READY`. Check for a Resume with the last sequence number. Send op `9`. Check for a fresh Identify.
- Deliver the same message ID twice. Check that one prompt is sent.
- Script a 5,000-character reply with `reply_chunk = 1500`. Check for four sends with `msg_seq` 1 to 4, the last ending with the cut-off note.
- Make the third passive send fail with `40034128`. Check that the rest go as active messages. Make those fail with `40034105`. Check that the text is sent first after the owner's next message.
- Send two messages without waiting. Check that each reply uses its own message's `msg_id`.
- Deliver a `GROUP_AT_MESSAGE_CREATE`. Check that the group's guest agent gets a prompt with a `qq-<member_openid>` sender and that the reply goes to the group endpoint with the mention's `msg_id`.
- Deliver a `GROUP_MESSAGE_CREATE` without mentions, one that mentions someone else, and one whose `mentions` has `is_you`. Check that only the last reaches a group agent, with the `<@…>` token of the bot removed.
- Send a reply to a fake QQ that accepts Markdown. Check one `msg_type: 2` message. Make it refuse Markdown. Check that the same reply follows as plain text with the next `msg_seq`, that the next reply skips Markdown, and that a scheduled reply is plain text.
- Deliver a C2C message with an image and a `voice` entry that has `voice_wav_url`, and one whose `voice` entry has only `asr_refer_text`. Check an `image` and an `audio` block, then the `[voice: …]` text. Deliver a 30 MB file entry. Check the too-large line and no download.
- Script a reply with two text parts and three `resource_link`s. Check that the upload and `msg_type: 7` sends use `msg_seq` 3 and 4, and that the third file goes as an active message. Make a type 3 upload fail with `850019`. Check the type 4 retry. Make a type 4 upload fail. Check the **Could not send** text. Script a 6 MB file. Check the 5 MB notice and no upload.
- In a trusted group, deliver mentions with `/new` and `/cancel`, and from the owner `/new friends`. Check `session/new` and `session/cancel` to the group's agent, **New session started.** to the group for the first, and the owner's reply for the last.
- List a group in `trusted_groups` and deliver a mention from it. Check that a group-mode agent gets the text with a `qq-<member_openid>` sender, `"owner": true` for an ID in `owner_member_openids`, that its approval reaches the owner marked with the group, and that the owner's `/yes` answers the group's agent, not the owner's.

With a real bot, which needs a registered bot, real-name QQ account, and network access, check what the documentation leaves unclear:

- Whether the `GROUP_AND_C2C_EVENT` intent works for an unpublished individual bot, and whether such a bot can join groups.
- Whether a person's `member_openid` is the same in every group. If not, they have notes of their own in each group, and the owner lists one ID per group in `owner_member_openids`.
- Whether the typing indicator counts as one of the four replies.
- The longest text QQ accepts in one message. Set `reply_chunk` below it.
- Whether active messages work for this bot, and their limits.
- Which fields a voice, video, and file attachment carry, whether `voice_wav_url` or `asr_refer_text` is among them, and the largest attachment QQ delivers.
- Whether the bot may send files of type 4, and which voice formats type 3 takes in a private chat.
- Ask a question in QQ. Check that the reply arrives, and that messages from a second QQ account get nothing.

[Architecture overview](README.md)
