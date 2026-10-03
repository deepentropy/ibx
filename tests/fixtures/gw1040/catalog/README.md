# Gateway 1040 catalogs

Tables read from the gateway 1040 JAR (its bytecode and its message table), for the layer A table tests
(ibx#485, `tests/gw_catalog.rs`). Every expected value of those tests comes from here, never from the ibx code.
Read on 24/09/2026 to 03/10/2026; the conditions come from the translation specs of the reference (order submit,
modify, cancel, combo and local validation), read in the bytecode.

## Files

| File | Rows | Columns |
|---|---|---|
| `error_codes.csv` | 612 | `code`, `message` (format string, `%s` placeholders; alternative texts joined by ` \|\| `), `wrapping`, `captured_text` |
| `order_local_rules.csv` | 168 | `id` (code and raise site), `code`, `api_code`, `stage`, `text`, `condition` |
| `order_writers.csv` | 973 | `msg` (`D` new order, `G` replace, `F` cancel), `pos`, `tag`, `block`, `site`, `when`, `condition` |
| `order_attributes.csv` | 328 | `tag`, `attribute`, `kind` |

### error_codes.csv

- `wrapping`: `as_is` (the code reaches the API client itself), `321` or `322` (always sent as the cause of
  321 "Error validating request" or 322 "Error processing request"), `321|as_is` (wrapped at some raise sites only).
- `captured_text`: the text the gateway was captured sending when it differs from its message table: 104 "Cannot
  modify a filled order.", 329 and 462 with the new value after a period, 413 and 10167 with one final period,
  443 with the parameter after a colon, 2174 with "Warning: " before it, 10148 "cannot". The tests take this text
  only.
- `wrapping` of 365 and 366 is `as_is`, read again in the bytecode (`jextend.bC.o()@101-112`,
  `jextend.bs.o()@90-101`: the code is sent itself); the index rule had them wrapped in 322.
- A line break of a message is written `\n` (two characters).
- `%SHORT_COMPNAME%`, `%SHORT_PRODNAME%` are the gateway's own placeholders (product and company names).

### order_local_rules.csv

One row per refusal the gateway makes before sending an order (and the modify and cancel refusals). `stage`:
`read` (the request decoder: the client gets 320 "Error reading request:" with the rule text, except the date
and time codes, which keep their code), `check` (first checks: 321 with the rule text after
`.-'bH' : cause - `), `build` (API order build), `rules` (order rules), `flow` (order id flow), `modify`,
`cancel`. `text` is what the API client gets; `%s` is a value of the request.

### order_writers.csv

The writers of the three order messages, flattened in bytecode order (`pos`). A tag listed at two positions is
two branches of the writer, not two emissions. A `block` (the order attributes, the algo parameters, the order
conditions, the combo legs, the FA allocation...) is one position whose tags come in any order: the attribute
block is a map keyed by class identity, so its order changes from one gateway run to the next. `when` says when
the gateway writes the tag, as the tests evaluate it on the API order:

| `when` | Meaning |
|---|---|
| `always` | always |
| `never` | never for an order placed through the API |
| `flag:<fact>` | when the API order has this fact (a limit price, an OCA group, a parent, conditions...) |
| `type_in:<types>` / `type_not_in:<types>` | for these API order types |
| `attribute` | the order attribute's own rule (not decided by the table) |
| `other` | a condition not decided by the table (order checked, presence not) |

Terms are joined by ` & `.

### order_attributes.csv

Each order attribute class with its tag and the kind of value its writer writes: `boolean` (only `=1`, never
`=0`), `integer`, `double` (2 to 8 decimals), `decimal`, `string`, `enum`, `datetime`, `character`, `custom`
(its own writer), `holder` (not written by the attribute loop: the order's main fields).

## Trademark terms

The texts and names that hold a trademark term carry the placeholder `%TM%` in its place: the messages of codes
2180, 10106, 10122, 10234, 10235 and 10247, the local rule of 10235, and the attribute name of tag 8015. The
tests read `%TM%` like `%s`: any text.
