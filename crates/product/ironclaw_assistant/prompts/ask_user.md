Ask the person a structured question and wait for their answer. They see a form in the chat, with your options as buttons, and the run pauses until they answer or dismiss it.

Use it when you need a decision only they can make and guessing would waste work: choosing between real alternatives, confirming an irreversible step, or filling in a detail you cannot find. Do not use it for things you can look up or decide yourself, and do not use it to ask permission for ordinary tool calls.

Ask one to four short questions at once. Give 2-6 concrete `options` when the answer is a choice (each `label` a few words, `description` for the trade-off); leave `options` out for a free-text answer. `allow_free_text` (default true) lets them type their own answer beside the options; set `multi_select` when several options can apply.

The result tells you what they chose. If it says they declined, do not ask the same question again; continue with your best judgment or ask what they want instead.
