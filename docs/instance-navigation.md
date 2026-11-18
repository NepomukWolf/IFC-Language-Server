# Instance Navigation and Model Queries

Besides the standard per-position features, the server answers questions about a document's
entity instances as a whole: the standard LSP call hierarchy walks the reference graph, and three
custom requests list, count and search instances. Ranges and positions follow LSP conventions
(zero-based lines, UTF-16 characters) unless stated otherwise.

The examples use this IFC4 document, opened as `file:///small.ifc`:

```text
#1=IFCBUILDINGSTOREY('2hQBAVPOr5VxhS3Jl0O47h',$,'Level 1',$,$,$,$,$,.ELEMENT.,0.);
#2=IFCWALL('3wdauVJT5Fx9drrREiDqA$',$,'North wall',$,$,$,$,$,$);
#3=IFCRELCONTAINEDINSPATIALSTRUCTURE('073e6zpmr80vpv2VZudDfO',$,$,$,(#2),#1);
```

## Call Hierarchy

`src/features/call_hierarchy.rs` implements `textDocument/prepareCallHierarchy`,
`callHierarchy/incomingCalls` and `callHierarchy/outgoingCalls` over local `#id` references:

- **prepare** returns the instance at the position: the one an `#id` token there names, or else
  the one whose text, from `#id` through its `;`, holds the position. It returns nothing outside
  every instance and for ids without a definition.
- **incoming calls** are the instances that reference the item, in document order. `fromRanges`
  are the `#id` tokens inside each referrer. At most 1000 instances are listed; past that, one
  last item stands for the references left out (see below).
- **outgoing calls** are the instances the item references, in the order of their first
  reference. `fromRanges` are the `#id` tokens inside the item. References to undefined ids are
  left out.

Objectified relationships are therefore intermediate nodes: a wall's incoming calls include its
`IfcRelContainedInSpatialStructure`, whose outgoing calls lead on to the storey and the other
contained elements.

Each item has `name` `#<id> <ENTITY>`, `kind` `Object` (19), `range` the instance's statement,
`selectionRange` its `#id`, and `detail` its `Name`, or else its `GlobalId`, when the selected
schema places those attributes and the instance has them as strings.

Every item's `data` has this shape, and the server reads `id` back from it when a client expands
the item:

```text
{
  id: number,
  entity: string,          // upper case
  attributes?: [{          // left out on prepared items
    position: number,      // one-based argument position
    name: string | null    // null when the schema or entity is unknown
  }]
}
```

`attributes` names the arguments of the referring instance that hold the call's references: on an
incoming call's `from` item, the arguments of `from`; on an outgoing call's `to` item, the
arguments of the item whose outgoing calls were requested.

When an instance has more than 1000 referrers, its incoming calls end with one more call whose
`from` item stands for the references left out. LSP call hierarchy has no paging, and a shared
instance such as an `IfcOwnerHistory` can have hundreds of thousands of referrers. That item has
`name` `… <count> more references not shown`, `kind` `Null` (21), `selectionRange` the first
reference left out, `range` the instance holding it, empty `fromRanges`, and this `data`:

```text
{ more: number }           // the number of references left out
```

Expanding it returns no calls in either direction.

Example: incoming calls of `#2`.

```json
[{
  "from": {
    "name": "#3 IFCRELCONTAINEDINSPATIALSTRUCTURE",
    "kind": 19,
    "detail": "073e6zpmr80vpv2VZudDfO",
    "uri": "file:///small.ifc",
    "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 77 } },
    "selectionRange": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 2 } },
    "data": {
      "id": 3,
      "entity": "IFCRELCONTAINEDINSPATIALSTRUCTURE",
      "attributes": [{ "position": 5, "name": "RelatedElements" }]
    }
  },
  "fromRanges": [{ "start": { "line": 2, "character": 69 }, "end": { "line": 2, "character": 71 } }]
}]
```

The call hierarchy uses the text index and scans only the instances it reports. It does not
load the statement index.

## Model Queries

The server registers these custom requests for the native binary and the wasm build alike. Each
returns `null` when the server has not opened `uri`.

### `ifc/instances`

`src/features/instances.rs`. Params: `{ uri, limit? }`. Lists the document's entity instances in
document order, the first `limit` of them when given, with every positional argument labelled by
its attribute name from the selected schema.

```text
{
  schema: string | null,   // the schema the names come from; null when unknown
  total: number,           // all instances, of which the first `limit` are listed
  instances: [{
    id: number | null,
    entity: string,
    range: Range,
    expectedAttributeCount: number | null,  // null when the schema or entity is unknown
    attributes: [{ name: string | null, value: Value }]
  }]
}
```

`Value` is `{ kind, range, ... }` with `kind` one of `reference` (`id`), `enumeration` (`value`),
`string` (`value`, unescaped), `number` (`value`, as written), `null` (`$`), `omitted` (`*`),
`list` (`items`), `typed` (`type`, `items`) and `unknown` for an argument the parser cannot type,
which keeps every later argument at its position. Ranges in `ifc/instances` use zero-based lines
and byte columns. The request reloads the statement index when necessary; there is no file-size
limit. Statement and value recovery follow the STEP parser described in `architecture.md`.

Example: `{ "uri": "file:///small.ifc", "limit": 1 }`, trimmed.

```json
{
  "schema": "IFC4",
  "total": 3,
  "instances": [{
    "id": 1,
    "entity": "IFCBUILDINGSTOREY",
    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 82 } },
    "expectedAttributeCount": 10,
    "attributes": [
      { "name": "GlobalId", "value": { "kind": "string", "value": "2hQBAVPOr5VxhS3Jl0O47h",
        "range": { "start": { "line": 0, "character": 21 }, "end": { "line": 0, "character": 45 } } } },
      { "name": "OwnerHistory", "value": { "kind": "null",
        "range": { "start": { "line": 0, "character": 46 }, "end": { "line": 0, "character": 47 } } } }
    ]
  }]
}
```

### `ifc/entityCounts`

`src/features/instance_queries.rs`. Params: `{ uri }`. Counts the instances of each entity, most
frequent first, then by name.

```json
{
  "schema": "IFC4",
  "total": 3,
  "entities": [
    { "entity": "IFCBUILDINGSTOREY", "count": 1 },
    { "entity": "IFCRELCONTAINEDINSPATIALSTRUCTURE", "count": 1 },
    { "entity": "IFCWALL", "count": 1 }
  ]
}
```

### `ifc/findInstances`

`src/features/instance_queries.rs`. Params: `{ uri, entity?, query?, limit }`. Lists, in document
order, the instances of `entity` (ignoring case) whose `GlobalId` or `Name` contains `query`
(ignoring case), the first `limit` of them. `globalId` and `name` are read where the selected
schema places them, so without a known schema they are `null` and a `query` matches nothing.

Example: `{ "uri": "file:///small.ifc", "query": "north", "limit": 5 }`.

```json
{
  "total": 1,
  "instances": [{
    "id": 2,
    "entity": "IFCWALL",
    "position": { "line": 1, "character": 0 },
    "globalId": "3wdauVJT5Fx9drrREiDqA$",
    "name": "North wall"
  }]
}
```

`ifc/entityCounts` and `ifc/findInstances` use the text index without loading the statement index.
