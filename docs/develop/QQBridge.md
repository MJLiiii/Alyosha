# Implement the QQ bridge

`alyosha-qq` relays private (C2C) messages between your QQ account and one `alyosha acp` child process, using QQ's official bot API v2. Build it after the Telegram bridge, reusing the shared bridge crate. [Chat channels design](../design/Channels.md#the-qq-bridge) · [Telegram bridge](TelegramBridge.md) · [ACP mode](Channels.md) · [Architecture overview](../README.md)

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
workspace = "/home/mjli/Projects/alyosha-qq"  # absolute, under ~/Projects, not the Telegram bridge's
reply_chunk = 1500                            # characters per message
secret = "..."                                # the AppSecret
```

Load it with `alyosha-config`, which refuses a file that others can read and checks every value. Refuse to start, with a clear error, if the `[qq]` section is missing. Never log the secret or an access token. [Configuration](../reference/Configuration.md#qq)

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

[WebSocket events](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/websocket.html) · [Payload and intents](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/payload.html)

## Incoming messages

Handle only `C2C_MESSAGE_CREATE` dispatches. In each, `d.id` is the message ID, `d.author.user_openid` the sender, `d.content` the text, and `d.attachments` any files. QQ may deliver the same message twice. Skip a message ID already seen in the last hour. [C2C message event](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/c2c_message_create.html)

Drop a message, logging the sender's `user_openid`, unless it equals `owner_openid`. Send nothing back.

For a message from the owner, trim the text, then act as the Telegram bridge does: `/new`, `/cancel`, `/compact`, `/job`, and `/tasks` behave the same, and other text becomes a `session/prompt`. Send `initialize` with `clientInfo.name` set to `alyosha-qq`, and start the task runner for the `qq` channel. Handle `_alyosha/job_finished` as the Telegram bridge does. Scheduled and job replies have no message to answer and follow their own delivery rules. [Delivering on QQ](Schedule.md#delivering-on-qq) A message with attachments and no text gets the reply **Only text messages work for now.** [Incoming messages](TelegramBridge.md#incoming-messages)

## Sending replies

Each prompt remembers the message ID it came from and a reply counter starting at 1. Send replies with `POST /v2/users/{owner_openid}/messages`, body `msg_type: 0`, `content`, `msg_id` set to the prompt's message ID, and `msg_seq` set to the counter, which increases with every send. The same `msg_id` and `msg_seq` twice fails. [Send a C2C message](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html)

QQ allows four replies per message within 60 minutes:

1. While the turn runs, send the typing indicator every 50 seconds: `msg_type: 6` with `input_notify: {"input_type": 1, "input_second": 60}` and the prompt's `msg_id`. Stop if QQ counts it as a reply. See verification.
2. When the prompt's response arrives, build the reply text as the Telegram bridge does, without the **Working…** message. [Showing progress and replies](TelegramBridge.md#showing-progress-and-replies)
3. Split it into parts of at most `reply_chunk` characters at line breaks. If more than four parts result, keep three and make the fourth the start of the remaining text, cut to fit, followed by **(cut off — ask me to continue)**.
4. Send the parts in order as passive replies.

If a send fails with `40034128` (reply limit or window exceeded), `40034005`, or `304103` (message ID expired), send the remaining parts as active messages: the same request without `msg_id`. If that fails too, for example with `40034100` (rate limit) or `40034105` (no permission), keep the unsent text. When the owner's next message arrives, send the kept text first, as that message's replies, before relaying the new message. Log every failed send with its error code.

On `40054007` (message too long), halve the part and retry once, and log it so `reply_chunk` can be lowered.

## Approvals and questions

On `session/request_permission`, send the request's `title`, a blank line, the `content` text, and then one line telling the owner how to answer:

- For two options: **Reply /yes for <first option> or /no for <second option>.**
- For more: a numbered list of the options, then **Reply with a number.**

Send it as a reply to the prompt's message, by the usual rules, or by the scheduled-reply rules for a scheduled prompt. Splitting counts against the four replies, so cut the body to fit and end it with **(cut off)**. The model formats nothing here: the title and body come from Alyosha. [Sending replies](#sending-replies) · [Delivering on QQ](Schedule.md#delivering-on-qq)

While a request is open, an owner message of exactly `/yes`, `/no`, or a valid number answers it with `selected` and the matching `optionId`. The bridge replies **Answered: <option name>** to that message, and later replies for the same prompt use that message's ID, which starts a fresh count of four. Any other message is a normal prompt. `/cancel` sends `session/cancel` first, then answers open requests with `cancelled`, as the Telegram bridge does. A `failed` update for the call means Alyosha stopped waiting. Reply **Expired — not approved.** to the next owner message.

If the request cannot be delivered by any means, answer it with `cancelled` at once, rather than keeping it for later, so Alyosha treats it as undelivered.

## Failures and shutdown

Follow the Telegram bridge: exit with a nonzero status when `alyosha acp` exits, after telling the owner if a reply is still possible. On SIGTERM, close the WebSocket, then the child's standard input, wait up to 10 seconds, and kill it. Retry network errors with backoff up to one minute. [Failures and shutdown](TelegramBridge.md#failures-and-shutdown)

## Run it in the container

Add a service next to `alyosha-telegram`, the same apart from its name and `ExecStart = "${alyosha}/bin/alyosha-qq"`. [Run it in the container](TelegramBridge.md#run-it-in-the-container)

## Verification

These run under `cargo test` with a fake QQ API, serving both HTTP and the WebSocket, and a fake ACP agent.

- Start without the `[qq]` section, and with `config.toml` readable by others. Check that each refuses and no secret or token appears in output.
- Start with an empty `owner_openid` and send a message. Check that nothing is relayed or sent back and the sender's `user_openid` is logged.
- Return a token reply with `expires_in` as a string, and one with a `code`. Check that the first works and the second fails clearly.
- Drop the WebSocket after `READY`. Check for a Resume with the last sequence number. Send op `9`. Check for a fresh Identify.
- Deliver the same message ID twice. Check that one prompt is sent.
- Script a 5,000-character reply with `reply_chunk = 1500`. Check for four sends with `msg_seq` 1 to 4, the last ending with the cut-off note.
- Make the third passive send fail with `40034128`. Check that the rest go as active messages. Make those fail with `40034105`. Check that the text is sent first after the owner's next message.
- Send two messages without waiting. Check that each reply uses its own message's `msg_id`.

With a real bot, which needs a registered bot, real-name QQ account, and network access, check what the documentation leaves unclear:

- Whether the `GROUP_AND_C2C_EVENT` intent works for an unpublished individual bot.
- Whether the typing indicator counts as one of the four replies.
- The longest text QQ accepts in one message. Set `reply_chunk` below it.
- Whether active messages work for this bot, and their limits.
- Ask a question in QQ. Check that the reply arrives, and that messages from a second QQ account get nothing.

[Architecture overview](../README.md)
