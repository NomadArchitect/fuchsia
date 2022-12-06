# FIDL examples

This is a catalog of FIDL examples intended to demonstrate FIDL concepts through
simplified implementations of real software workflows.

## Example index

The following examples sequentially demonstrate useful FIDL concepts.

<!-- DO_NOT_REMOVE_COMMENT:examples (Why? See: /tools/fidl/scripts/canonical_example/README.md) -->

### Calculator

The [calculator][example_calculator] example shows fundamental building blocks
for creating your first FIDL protocol.

### Key-value store

The [key-value store][example_key_value_store] example demonstrates how to build
a simple key-value store using FIDL in order to learn about the various data
types available in the language.

### Canvas

The [canvas][example_canvas] example demonstrates how to build a simple 2D
line-rendering canvas using FIDL in order to learn about commonly used data flow
patterns.

<!-- /DO_NOT_REMOVE_COMMENT:examples (Why? See: /tools/fidl/scripts/canonical_example/README.md) -->

## Concept index

Each "concept" in the FIDL language is exemplified in at least one of the
examples listed in the preceding section. A quick reference of each such
concept, as well as its example implementations, is listed in the following
section.

<!-- DO_NOT_REMOVE_COMMENT:concepts (Why? See: /tools/fidl/scripts/canonical_example/README.md) -->

<<../widgets/_acknowledgement_pattern.md>>

<<../widgets/_alias.md>>

<<../widgets/_anonymous_type.md>>

<<../widgets/_bits.md>>

<<../widgets/_discoverable.md>>

<<../widgets/_enum.md>>

<<../widgets/_feed_forward_pattern.md>>

<<../widgets/_generated_name.md>>

<<../widgets/_handle_rights.md>>

<<../widgets/_infallible_two_way_method.md>>

<<../widgets/_named_payload.md>>

<<../widgets/_optionality.md>>

<<../widgets/_pagination_pattern.md>>

<<../widgets/_persistence.md>>

<<../widgets/_protocol_end.md>>

<<../widgets/_protocol.md>>

<<../widgets/_recursive_type.md>>

<<../widgets/_resource_type.md>>

<<../widgets/_scalar_type.md>>

<<../widgets/_size_constraint.md>>

<<../widgets/_struct_payload.md>>

<<../widgets/_table_payload.md>>

<<../widgets/_throttled_event_pattern.md>>

<<../widgets/_union_payload.md>>

<!-- /DO_NOT_REMOVE_COMMENT:concepts (Why? See: /tools/fidl/scripts/canonical_example/README.md) -->

[example_calculator]: calculator/README.md
[example_canvas]: canvas/README.md
[example_key_value_store]: key_value_store/README.md
