# Implement the Telegram bridge

`alyosha-telegram` relays one Telegram private chat to one `alyosha acp` child process. Build it after the ACP mode passes its checks. [Chat channels design](Channels.md#the-telegram-bridge) · [ACP mode](Channels.md#implementation) · [Architecture overview](README.md)

## Package layout

Turn the repository root into a Cargo workspace with these packages:

| Folder | Package | Holds |
| --- | --- | --- |
| root | `alyosha` | Alyosha itself, including `alyosha acp` |
| `bridge/` | `alyosha-bridge` | A library shared by the bridges: starting `alyosha acp`, `initialize`, opening a session by key with `session/load` or `session/new`, sending prompts, attributing updates to the oldest open prompt, collecting each prompt's tool calls, text, files to send, and outcome, building attachment blocks, and the pool of guest sessions for group chats |
| `telegram/` | `alyosha-telegram` | The Telegram side |
| `qq/` | `alyosha-qq` | The QQ side. [QQ bridge](QQBridge.md) |
| `config/` | `alyosha-config` | The configuration schema and loader, used by every program. [Configuration](reference/Configuration.md#implementation) |
| `schedule/` | `alyosha-schedule` | The scheduled-task file, used by the bridges' task runner, and the schedule MCP server. [Scheduled tasks](Schedule.md#implementation) |

No bridge depends on Alyosha's library. `alyosha-bridge` depends on the same pinned `agent-client-protocol` version as Alyosha, in its client role. `alyosha-telegram` calls the Bot API directly with `reqwest`: it needs only `getUpdates`, `sendMessage`, `editMessageText`, `answerCallbackQuery`, `sendChatAction`, `setMyCommands`, `getFile` with the file download URL, and `sendPhoto`, `sendVideo`, `sendAnimation`, `sendVoice`, `sendAudio`, and `sendDocument` for files, and handles `429` and network retries itself. `ALYOSHA_TELEGRAM_API` points it at a fake server in tests. [`teloxide`](https://docs.rs/teloxide) was the first choice and adds nothing these calls need. The flake builds every binary into the `alyosha` package. Build the Telegram bridge first and move its ACP side into `alyosha-bridge` when starting the QQ bridge.

## Configuration

```toml
[telegram]
owner_id = 123456789                            # your numeric Telegram user ID

[telegram.trusted_groups]                       # optional: groups with your powers
family = -1001234567890                         # folder name = chat ID
```

In Nix, this is `services.alyosha.settings.telegram`. Load it with `alyosha-config`, which checks every value, and read the token from the secret `telegram-token`, set with `alyosha secret set telegram-token`. Refuse to start, with a clear error, if the `[telegram]` section or the secret is missing. Never log the token. [Configuration](reference/Configuration.md#telegram) · [Secrets](reference/Configuration.md#secrets)

Create the bot with Telegram's @BotFather, which issues the token. Get your user ID from the bridge's own log: it logs the sender ID of every message it drops.

## Startup

1. Read the configuration and the token.
2. Start `alyosha acp` from `PATH` with piped standard input and output. Send its standard error to the bridge's own standard error, which the journal keeps, for failures before its logging is set up. Both programs write their own log files. [Logging](reference/Logging.md)
3. Send `initialize` with protocol version `1`, `clientInfo.name` set to `alyosha-telegram`, and no client capabilities, then open the session `owner:telegram` in `/var/lib/alyosha/work/owner`, creating it if missing, with an empty `mcpServers` list: `session/load`, or `session/new` if there is none. [Durable sessions: bridges](SessionStore.md#bridges)
4. Call `getMe` and keep the bot's user ID and username, which group mentions name. Network errors are retried as for every call; if Telegram refuses it, as for a wrong token, exit with an error.
5. Call `setMyCommands` with `new`, `cancel`, `compact`, `job`, and `tasks`, each with a one-line description. For each trusted group, call it again with `new`, `cancel`, `compact`, and `job` and the scope `{"type": "chat", "chat_id": <ID>}`, so the menu appears only there.
6. Start long polling with `getUpdates`, asking only for `message` and `callback_query` updates.
7. Start the task runner for the `telegram` channel. [The runner](Schedule.md#the-runner)
8. On each `_alyosha/job_finished` notification, send **⚙ Job finished:** to the chat, then send the notification's `text` as a `session/prompt`. [When a job finishes](Jobs.md#when-a-job-finishes)

Updates that arrived while the bridge was down are delivered on startup and processed in order. Telegram keeps them for 24 hours.

## Incoming messages

A message in a `group` or `supergroup` chat goes to [group chats](#group-chats). Drop any other update, with a log line naming the sender ID and chat type, unless the chat is private and the sender's ID equals `owner_id`. Send nothing back.

For a message from the owner:

| Message | Action |
| --- | --- |
| `/start` | Reply with a short help text listing the commands. |
| `/new` | Send `session/new` in `/var/lib/alyosha/work/owner` with the key `owner:telegram`. Reply **New session started.** once it returns, which can take minutes while a project's dev shell builds. [Sessions](Channels.md#sessions) |
| `/cancel` | Send `session/cancel` for the current session. |
| `/new <name>`, `/cancel <name>` | Do the same for the trusted group `<name>` in `trusted_groups`, and reply to the owner, such as **New session started in group dev.**; reply **No trusted group named <name>.** to an unknown name. [Commands in trusted groups](Channels.md#commands-in-trusted-groups) |
| `/compact`, `/job …` | Send it as the text of a `session/prompt`. [`/job` command](Jobs.md#job-command) |
| `/tasks` | List scheduled tasks from the file, without the model. [`/tasks`](Schedule.md#tasks) |
| Other text, or a message with attachments | Send it as a `session/prompt`: the text or caption as one `text` block, then a block for each attachment. [Attachments](#attachments) |
| Anything else, such as a game or a story | Reply **Only text, photos, voice, video, stickers, and files work for now.** |

Send prompts at once, without waiting for earlier ones, except that an album waits for its other messages. [Attachments](#attachments) Alyosha queues them. Track open prompts in order. ACP updates name only the session, so attribute each update to the oldest open prompt, which is the one whose turn is running.

## Attachments

Build the prompt's blocks with the shared crate's `attachments` module, in this order: the text or caption, the replied-to message's part if any, then this message's attachments. [From the bridge to `alyosha acp`](Media.md#from-the-bridge-to-alyosha-acp)

| Telegram field | Block |
| --- | --- |
| `photo` | `image`, from the largest size |
| `document` | `image` if its MIME type is `image/jpeg`, `image/png`, or `image/webp`, otherwise `resource` with its `file_name` |
| `voice`, `audio` | `audio`, with `audio`'s `file_name` in `_meta` |
| `video`, `video_note`, `animation` | `resource`, with the file name if Telegram gives one. Use `animation` rather than the `document` Telegram also sends with it. |
| `sticker` | A `text` block `[sticker <emoji>]`, then `image`: the sticker itself if it is static, its `thumbnail` if `is_animated` or `is_video`, or nothing if it has no thumbnail |
| `location`, `venue` | `text`: `[location: <latitude>, <longitude>]`, with the venue's title and address after a comma |
| `contact` | `text`: `[contact: <first and last name>, <phone number>]` |
| `poll` | `text`: `[poll: <question> — <option> / <option> / …]` |
| `dice` | `text`: `[dice <emoji>: <value>]` |

Download a file with `getFile`, then its `file_path` from `https://api.telegram.org/file/bot<token>/`. Do not download a file whose `file_size` is over 20 MB: send the text `[<kind> <file name> too large to download: <size> MB]` in its place. Never log the download URL, which holds the token.

- **Albums.** Messages with the same `media_group_id` are one album. Hold each until no message of that album has arrived for one second, then send them as one prompt in message order, with their captions joined by blank lines. Keep at most ten attachments; beyond that, add `[<n> more attachments left out]`.
- **Replies.** If the message replies to a message not from the bot, in a private chat or a group, put `(Replying to <name>: "<first 500 characters of its text or caption>")` and a line break in front of the text, and add that message's attachments after the text, by the same rules. Its sender's name is the display name as for group senders.

## Showing progress and replies

While any prompt is open, send the `typing` chat action every 5 seconds.

For each prompt, on its first `tool_call` update, send a message reading **Working…** followed by the call's title. Append each later call's title as a new line, and mark failed calls with **(failed)**. Edit that message at most once every 3 seconds, sending the latest text, so the chat stays within Telegram's rate limits. Keep only the last 20 lines.

Collect the prompt's `agent_message_chunk` text in order. When the prompt's response arrives:

| Response | Message |
| --- | --- |
| `stopReason: "end_turn"` | The collected text. If it is empty, **Done.** |
| `stopReason: "cancelled"` | Any collected text, then **Stopped.** |
| Any other stop reason | Any collected text, then the stop reason. |
| JSON-RPC error | **Failed:** and the error message. |

The model writes Markdown. Split the reply into parts of at most 4000 characters at the last line break before the limit, or at the limit if a line is longer. When a cut falls inside a fenced code block, close the fence at the end of the part and open it again, with its language, at the start of the next. Convert each part to Telegram's HTML subset and send it with `parse_mode: "HTML"` and link previews off: `<b>` for bold and headings, `<i>`, `<s>`, `<code>`, `<pre><code class="language-…">` for code blocks, `<a href>`, and `<blockquote>` for the outermost quote. Lists become `• ` or `1. ` lines, and table rows become cells joined by ` | `. Escape `&`, `<`, and `>` in text. If Telegram refuses a part, for example because it cannot parse the entities, log it and send the same part again without a parse mode, with the Markdown markers removed. The conversion lives in the shared crate's `markdown` module. [Formatting options](https://core.telegram.org/bots/api#formatting-options)

The **Working…** message, approvals, and the bridge's own notices stay plain text.

### Sending files

A prompt's `resource_link` chunks are files from `send_file`. Collect them in order and send them after the reply, as multipart uploads, with the link's `description` as the caption, cut to 1,024 characters. [Sending files out](Media.md#sending-files-out)

| MIME type | Method |
| --- | --- |
| `image/jpeg`, `image/png`, `image/webp`, at most 10 MB | `sendPhoto` |
| `image/gif` | `sendAnimation` |
| `video/mp4` | `sendVideo` |
| `audio/ogg` | `sendVoice` |
| `audio/mpeg`, `audio/mp4` | `sendAudio` |
| anything else | `sendDocument` |

If Telegram refuses a photo, video, animation, voice, or audio upload, send the same file once more with `sendDocument`. If that fails too, send **Could not send <name>:** and the error. Delete the copy after either. A group reply's files go to the group like its text.

## Approvals and questions

On `session/request_permission`, send a message with the request's `title`, a blank line, and the `content` text, split like a reply. Attach an inline keyboard to the last part with one button per option, in order, each on its own row, with the option's `name` as its label and its `optionId` as callback data prefixed by a short request number, such as `r7:0`. [Approvals design](Approvals.md) · [The ACP mode](Approvals.md#the-acp-mode)

- **A button press from the owner** answers the request with `selected` and that `optionId`. Call `answerCallbackQuery`, then edit the message to remove the keyboard and add **Answered: <option name>**.
- **A button press from anyone else**, or on a request already answered or expired, gets only `answerCallbackQuery` with no change.
- **A `tool_call_update` with `failed`** for the request's call ID means Alyosha stopped waiting. Remove the keyboard and add **Expired — not approved.**
- **`/cancel`** sends `session/cancel` first, then answers every open request with `cancelled`, as ACP requires. Alyosha then treats the request as stopped, not undelivered, and ends the turn. [The ACP mode](Approvals.md#the-acp-mode)
- **Other messages** are sent as prompts as usual and wait in Alyosha's queue. They do not answer the request.

If the message cannot be sent after retries, answer the request with `cancelled`, which Alyosha treats as undelivered.

## Group chats

Keep the bot's privacy mode on, as @BotFather sets it, so Telegram passes the bot only group messages that mention it, reply to it, or are commands. Leave **Allow groups** on. [Group chats design](Channels.md#group-chats)

A group message is a **mention** when its `entities`, or `caption_entities` for a photo or file with a caption, hold a `mention` whose text is `@` and the bot's username, ignoring case, or a `text_mention` of the bot's user ID, or when it replies to a message from the bot. Drop every other group message without a log line, including commands, which privacy mode also passes, except a command in a trusted group. Remove the mention text and trim. If the message replies to someone else's message, bring its text and attachments along as in [Attachments](#attachments).

For a mention in a group whose chat ID is in `trusted_groups`, see [Trusted groups](#trusted-groups). For any other mention:

1. Take the group's guest session from the pool, starting `alyosha acp --guest` for it if none runs: `initialize` with `alyosha-telegram`, then open the session `guest:telegram:<chat ID>` with `/var/lib/alyosha/work/guest` as `cwd`, which guest mode ignores.
2. Send the text and attachments as a prompt with the sender `telegram-<from.id>` and the display name, the first and last name, or the username if there is none. The guest session decides what it reads. [Guests](Media.md#guests)
3. While the group's prompt is open, send `typing` to the group every 5 seconds.
4. When the response arrives, send the reply to the group rendered and split like a private reply, with `reply_parameters` naming the mention and `allow_sending_without_reply`. A failed send is logged and dropped. Guest sessions send no tool calls worth showing, so there is no **Working…** message.

The pool keeps one guest child per group ID. Stop a child, closing its input as at shutdown, when its group has had no open prompt for 60 minutes. If a child exits, log it and remove it from the pool; the next mention starts a new one. A guest child never stops the bridge. Log the chat ID of every mention at info level, so the owner can copy it into `trusted_groups`.

### Trusted groups

For a mention in a trusted group: [Trusted groups design](Channels.md#trusted-groups)

1. Take the group's session from the pool, starting `alyosha acp --group` for it if none runs: create `/var/lib/alyosha/work/groups/<name>` if missing, where `<name>` is the group's entry in `trusted_groups`, `initialize` with `alyosha-telegram`, then open the session `group:telegram:<chat ID>` in that folder.
2. Send the text with the sender in `_meta`, as for a guest group, adding `"owner": true` when the sender's ID is `owner_id`. Alyosha puts the name and the person's notes in front. Typing and the reply work as for a guest group. [People in group chats](Memory.md#people-in-group-chats)
3. **Approvals.** Send the request to the owner's private chat as usual, with **In group <name>:** and a blank line in front of the title. Give it a request number of its own, so a button press answers the right child. `/cancel` in the private chat cancels only the private session and its requests; a group's requests stay open until answered or expired.
4. **Jobs.** On a finished job, send **⚙ Job finished:** to the group, prompt the group's session with the result, and send its reply to the group without `reply_parameters`.
5. **Commands.** A message whose whole text, with any mention removed, is `/new`, `/cancel`, `/compact`, or `/job …`, or `/new@<username>` and the like without a mention, is a command for the group's session, from anyone there. `/new` sends `session/new` with the key `group:telegram:<chat ID>` in the group's folder and replies **New session started.** to the group. `/cancel` sends `session/cancel` to the group's child, answers its open requests with `cancelled`, and edits their messages in the owner's chat to end with **Cancelled.** `/compact` and `/job …` are sent as the bare text of a `session/prompt`, without the name in front. [Commands in trusted groups](Channels.md#commands-in-trusted-groups)
6. The pool never stops a trusted group's child for being idle. If it exits, log it and remove it; the next mention starts a new one.

## Failures and shutdown

- **`alyosha acp` exits.** Send **Alyosha stopped unexpectedly. Restarting.**, then exit with a nonzero status. systemd restarts the bridge, which starts a new child and loads the same session.
- **Telegram API errors.** On a `429` reply, wait the `retry_after` it gives. On other network errors, retry with backoff up to one minute. Never drop a reply because of a temporary error.
- **SIGTERM.** Stop polling, close the child's standard input, wait up to 10 seconds for it to exit, then kill it.

## Run it in the container

Add a service to the container's `config`, next to `environment.systemPackages`. [Host module](Deployment.md#host-module)

```nix
systemd.services.alyosha-telegram = {
  wantedBy = [ "multi-user.target" ];
  wants = [ "network-online.target" "gproxy.service" ];
  after = [ "network-online.target" "gproxy.service" ];
  # Reach the commands in environment.systemPackages, such as MCP servers.
  path = [ "/run/current-system/sw" ];
  serviceConfig = {
    User = "alyosha";
    ExecStart = "${alyosha}/bin/alyosha-telegram";
    Restart = "on-failure";
    RestartSec = 5;
  };
};
```

Alyosha finds its folders at their fixed paths. Follow the bridge, its `alyosha acp`, and their crashes with `alyosha logs telegram -f` on the host. [Logging](reference/Logging.md)

The bridge's `alyosha acp` uses the same GPROXY service as the QQ bridge's, so both can run at once. [Run GPROXY as a service](LLMClientGateway.md#run-gproxy-as-a-service)

## Verification

These run under `cargo test` with a fake Bot API server, set through `teloxide`'s API URL, and a fake ACP agent that the test scripts.

- Start without the `[telegram]` section, and without the `telegram-token` secret. Check that each refuses with a clear error and the token never appears in output.
- Send a private message from another user. Check that the agent receives nothing, nothing is sent back, and the sender ID is logged.
- In a group, send a message without a mention, then one from a stranger that mentions the bot, then one that replies to the bot's message. Check that the first is dropped, that a second, guest agent receives the other two as prompts with the mention removed and `telegram-<id>` senders, that the owner's agent receives nothing, and that each reply goes to the group with `reply_parameters`.
- Mention the bot in two groups. Check that each group gets its own guest agent.
- List one group in `trusted_groups` and mention the bot there. Check that a group-mode agent starts in `/var/lib/alyosha/work/groups/<name>`, receives the text with the sender in `_meta`, and that its reply goes to the group. Mention it from the owner's account. Check `"owner": true` on that sender. Script an approval from it. Check that the question reaches the owner's private chat marked with the group, that the owner's button press answers the group's agent, and that the owner's agent receives nothing.
- Let the trusted group's agent sit idle past the guest limit. Check that it keeps running.
- In the trusted group, send `/new@<username>`, then a mention with `/compact`, then a mention with `please /compact later`. Check a `session/new` with the group's key and folder and **New session started.** in the group, a bare `/compact` prompt, and `<name>: please /compact later`. Script an approval from the group, then send `/cancel` in the group. Check `session/cancel` to the group's agent, the request answered `cancelled`, and **Cancelled.** in the owner's chat. Send `/new` in a guest group with a mention. Check that the guest agent gets it as text.
- From the owner's chat, send `/new dev`, `/cancel dev`, and `/new nosuch`. Check `session/new` and `session/cancel` to the group's agent, the replies in the owner's chat, nothing in the group, and **No trusted group named nosuch.**
- At startup, check a `setMyCommands` call with the chat scope for each trusted group.
- Send an owner message. Check that the agent receives one `session/prompt` with the text and that the reply is sent with `parse_mode: "HTML"`.
- Script a reply with bold, code, and a list. Check the HTML that is sent. Make the fake Bot API refuse a reply's HTML. Check that the same part is sent again without a parse mode and without the Markdown markers.
- Script two tool calls, the second failing, then a reply. Check that one **Working…** message is created and edited, shows **(failed)** on the second line, and is edited no more than once every 3 seconds.
- Script a 9,000-character reply. Check that three messages arrive in order and each is under 4,000 characters.
- Send two messages without waiting. Check that both prompts are sent at once and that each reply goes out when its own response arrives.
- Send `/cancel` and `/new`. Check for `session/cancel` and `session/new`. Send `/cancel` while an approval is open. Check that `session/cancel` is sent before the request is answered with `cancelled`.
- Send a photo with a caption. Check one prompt with a `text` block and an `image` block holding the largest size. Send a voice message, a video, an animation, and a PDF. Check an `audio` block, two `resource` blocks with their file names, the animation sent once, and a `resource` block for the PDF.
- Send an album of three photos with one caption, then a text right after. Check that the album arrives as one prompt with three images before the text's prompt.
- Send a document whose `file_size` is 30 MB. Check that no download happens and the prompt holds the too-large line.
- Send an animated sticker, a location, and a poll. Check the thumbnail with the emoji line, and the two text lines.
- Reply to a forwarded photo with a question. Check the replying line and the photo after the text.
- Send a game. Check the unsupported reply and no prompt.
- Script a reply with a `resource_link` to a PNG and one to a ZIP. Check that the reply text goes first, then `sendPhoto` with the caption, then `sendDocument`. Make the fake Bot API refuse the photo. Check the `sendDocument` retry and that both copies are deleted.
- Make the fake Bot API reply `429` with `retry_after`. Check that the bridge waits and the reply still arrives.
- Make the agent exit. Check for the unexpected-stop message and a nonzero exit status.

With a real bot, which needs a token and network access:

- Run the bridge in the container with the real `alyosha acp`. Ask it to write a file and read it back. Check that the tool call and the reply appear in Telegram, and that the file is in `/var/lib/alyosha/work/owner` in the container.
- Message the bot from a second account. Check that nothing comes back.
- Add the bot to a group. From the second account, mention it, tell it your favorite color, and later ask for it. Check the answer and the person's row in `user_notes`.
- Add the group's chat ID from the log to `trusted_groups` under a name and restart. From the second account, ask it to run `pwd`. Check that it answers `/var/lib/alyosha/work/groups/<name>` and that a command in `ask` asks you in your private chat.
- Restart the container. Check that the service comes back on its own.

[Architecture overview](README.md)
