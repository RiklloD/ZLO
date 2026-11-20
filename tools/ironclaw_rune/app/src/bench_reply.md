## How the pacer and the fold fit together

The gateway delivers a reply in **bursts**: a few words, a pause, then a
sentence at once. The chat never shows a burst as it lands. Instead the
`pace.rs` pacer keeps a *target* and a *shown* length and steps between them
on a 30 Hz clock, so the reader sees words arrive at a steady rate.

Each step goes through the same path:

1. `on_stream` applies the frames to the transcript.
2. `refresh_rows` rebuilds the rows from it.
3. `sync_items` asks the pacer what to show and folds the rows into items.
4. The view diffs the new items against the old ones by key.

A few rules keep this cheap:

- finished Markdown blocks are memoised, so only the **last block** reflows
- code blocks are laid out line by line
- the working line only rebuilds when its shown second turns
- a burst that changes nothing costs no frame at all

| stage | runs on | cost per step |
|---|---|---|
| apply | UI thread | a few µs |
| fold | UI thread | proportional to rows |
| layout | UI thread | the changed block |
| paint | UI thread | dirty quads only |

Here is the shape of the fold, simplified:

```rust
pub fn fold(rows: &[Row], live: bool) -> Vec<Item> {
    let mut items = Vec::with_capacity(rows.len());
    let mut group: Vec<Row> = Vec::new();
    for row in rows {
        match row {
            Row::Tool { .. } | Row::Thinking { .. } => group.push(row.clone()),
            other => {
                if !group.is_empty() {
                    let key = format!("act-{}", group[0].key());
                    items.push(Item::Activity { key, rows: std::mem::take(&mut group), live });
                }
                items.push(Item::Row(other.clone()));
            }
        }
    }
    if !group.is_empty() {
        let key = format!("act-{}", group[0].key());
        items.push(Item::Activity { key, rows: group, live });
    }
    items
}
```

When the run ends the pending line gives way to the answer, the reserve
under the prompt shrinks to the answer's height, and the list goes back to
following its end. Nothing should jump: the prompt stays where it was drawn
across that commit, and the answer's last line lands on a whole word.
