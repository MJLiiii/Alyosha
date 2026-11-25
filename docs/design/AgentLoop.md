# Agent Loop

The Agent Loop decides what happens next in a session. Its **session driver** chooses the next message or command. Its **turn driver**, `run_turn`, takes one user message through as many model requests and tool batches as it needs to produce an answer.

## User-message turn

1. **Start the turn.** Take the oldest waiting message, pin the selected model for this turn, and append the message to Agent State. Messages that arrive during this turn wait for their own turns. A background job that finishes adds a message to the same queue, which starts a turn without you. [Agent State](AgentState.md) · [Background jobs](Jobs.md)

   The message goes into history with a **clock line** in front, such as `[Wed 2026-11-25 10:02, Europe/Amsterdam]`, giving the time you sent it in your configured timezone. That is how the model knows the date for requests like "remind me Friday" or "tomorrow's meetings". The system instruction stays fixed for the session so that provider prompt caching keeps working, so the clock cannot go there. The TUI and the bridges show your message without the line. [Configuration](../reference/Configuration.md#top-level)
2. **Prepare and send a request.** Get the available tool definitions from the Tool Registry. Ask the Context Manager to fit the conversation and definitions into the request budget, then send the prepared request through the LLM Client / Gateway. [Tool Registry](ToolRegistry.md) · [Context Manager](ContextManager.md) · [LLM Client / Gateway](LLMClientGateway.md)
3. **Read the complete response.** Record the assistant’s text and tool calls together in their original order. If there are zero calls, return the reply to the user and finish the turn. A clarifying question also ends the turn.
4. **Run the calls in order.** Give each call to the Tool Executor. Record its result under its original call ID, including any tool error. After the whole batch, go back to step 2 so the model can use the results. [Tool Executor](ToolExecutor.md)

For example, if the model calls `read_file` with `{"path": "notes.md"}`, the executor returns the file text or a read error. The **same turn** continues with that result. If the model then answers the question, the loop saves the answer once and returns it for display. The implementation guide traces a complete two-call exchange using a scripted model. [Turn verification](../develop/AgentLoop.md#verification-with-a-scripted-model-and-fake-tools)

## Commands and interruptions

Each session has one active slot. Bare `/compact` can claim it only when the session is idle and the message queue is empty. The driver delegates compaction to the Context Manager and displays the command’s status outside the conversation. A busy session reports that the command can be tried again when idle. [Manual compaction](ContextManager.md#compact-command) `/job <command>` needs no slot: the driver starts the background job at once, even during a turn, and reports it outside the conversation. [Background jobs](Jobs.md#starting-a-job)

**Stop** signals the active turn or command. During generation the loop discards any partial model reply. During tools it records the active call’s outcome and a cancelled result for every unstarted call. A running file operation finishes and returns its actual result. A running shell command is killed, and its result keeps the output so far with the status `Stopped`. A running `execute_code` script stops and its host waits for any active read before returning cancellation. A running MCP call is cancelled at the server and recorded as cancelled at once. [MCP Client](MCP.md#stop-timeouts-and-failures) After success, Stop, or failure, the driver releases the slot and starts the oldest waiting message. [Tool cancellation](ToolExecutor.md#stop-and-cleanup)

[Architecture overview](../README.md) · [Agent Loop implementation guide](../develop/AgentLoop.md)
