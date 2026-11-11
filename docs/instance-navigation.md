# Instance Navigation

Besides the standard per-position features, the server walks the reference graph between a
document's entity instances through the standard LSP call hierarchy. Ranges and positions follow LSP conventions
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
  are the `#id` tokens inside each referrer.
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

The call hierarchy uses the text index and scans only the instances it reports, so it works on
files above the AST parsing limit and its cost does not grow with file size.
