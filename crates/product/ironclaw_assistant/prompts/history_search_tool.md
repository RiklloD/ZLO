Search the person's past chats (their own messages and your finished replies; tool output is not included). The mode follows from the arguments:
- discover: `query` → the best-matching threads, each with highlighted snippets and message ids (add `thread_id` to search one thread);
- scroll: `thread_id` + `around_message_id` (or `around_sequence`) → the messages around a hit, `window` on each side;
- read: `thread_id` alone → that thread's messages oldest first, bounded; continue with `from_sequence` = `next_from_sequence`;
- browse: no arguments → the most recent threads.
Use it when the person refers to something said in an earlier chat. Automation and subagent threads rank below the person's own threads. If `index_complete` is false, older chats are still being indexed — search again shortly.
