Working fast and precisely with these tools:
- Find a control with `query` (text and/or role) instead of reading a whole tree when you know what you are looking for; its indexes work with click, set_value, scroll and perform_secondary_action.
- Action results show only the tree lines that changed ("tree diff"); indexes on unchanged lines stay valid. A full tree comes back when the window changed or many lines moved.
- Multi-window apps (mail compose, several browser windows): the `Windows:` header line lists window ids. Pin the one you are working in with get_app_state window_id (or window_title) so actions and results cannot drift to another window.
- set_value and type_text read the field back. A set_value error or a type_text WARNING means nothing was written: do not assume success and do not retype blindly; check state first.
- Long or multi-line text (email bodies): type_text method="paste". If type_text says no focused field was found, click the field first.
- Before saving, sending or submitting, verify from the app itself (for mail: reopen the draft and check recipients, subject and body).
