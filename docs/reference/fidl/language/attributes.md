# FIDL attributes

The following FIDL attributes are supported:

* [`@available`](#available)
* [`@deprecated`](#deprecated)
* [`@discoverable`](#discoverable)
* [`@doc`](#doc)
* [`@generated_name`](#generated-name)
* [`@no_doc`](#nodoc)
* [`@selector`](#selector)
* [`@transitional`](#transitional)
* [`@transport`](#transport)
* [`@unknown`](#unknown)

## Scope

An attribute precedes a FIDL element, for example:

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/attributes.test.fidl" region_tag="attribute-one" %}
```

It's used to either modify the characteristics of the element, or provide
documentation.

Note: The attribute applies *only* to the *next* element, not all
subsequent ones.
Elements after the current one revert to having no attributes.

## Syntax

Attributes may include values, and multiple attributes may be stacked, for example:

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/attributes.test.fidl" region_tag="attribute-many" %}
```

Illustrates both aspects:

* there are two attributes, `@discoverable` and `@transport`, and
* the `@transport` attribute takes a value from the list enumerated below.

## `@available` {#available}

**USAGE**: `@available(platform="`_string_`", added=`_version_`,
deprecated=`_version_`, removed=`_version_`, replaced=`_version_`,
renamed="`_string_`", note="`_string_`")`

**MEANING**:
All arguments are optional, but at least one of `added=`, `deprecated`,
`removed`, or `replaced` must be provided.

* `platform`: Only allowed when the attribute is on the `library` declaration.
  Must be a valid [library name element][identifiers]. If omitted, defaults to
  the first element of the library name.
* `added`, `deprecated`, `removed`, `replaced`: Must be an integer from 1 to
  2^31-1 (inclusive), or one of the special constants `NEXT` and `HEAD`.
    * `removed` and `replaced` are mutually exclusive.
    * Must respect `added <= deprecated < removed` or
      `added <= deprecated < replaced`.
* `renamed`: Only allowed if `removed` or `replaced` is provided. Specifies the
  new name of the element after it is removed or replaced.
* `note`: Must be provided if and only if `deprecated`, `removed`, or `replaced`
  is provided. Should contain a brief explanation indicating what to use
  instead that is suitable to be included in compiler warning or error messages.

See [FIDL versioning](versioning.md) for more details.

## `@deprecated` {#deprecated}

Note: Obsolete. Use [`@available`](#available).

**USAGE**: `@deprecated`

**MEANING**:
See [RFC-0058].

## `@discoverable` {#discoverable}

**USAGE**: `@discoverable(name=`_name_`)`

**MEANING**:
Assigns a name to use for service discovery. That is to say, a `@discoverable`
protocol can be served under the given name, and clients that connect to
that protocol can search for it under the same name. This makes it possible to
have a client search for the correct name without manually ensuring that the
lookup name matches the one passed on the server side.

If _name_ is omitted, a name is generated based on the library and protocol name
(e.g. `some.library.SomeProtocol`). If _name_ is provided, it must follow the
same format. Note that this is **not** the [fully qualified name], which would
use a slash (e.g. `some.library/SomeProtocol`).

The `@discoverable` name is also used by `fuchsia.unknown/Queryable` to enable
dynamic protocol resolution for type erased channels. For example, if a client
has an untyped [Zircon channel][channel] but knows the server implements
`Queryable`, the `fuchsia.unknown/Queryable.Query` method can be used to obtain
the concrete protocol type.

## `@doc` {#doc}

**USAGE**: `@doc("`_string_`")`

**MEANING**:
In FIDL, comments can start with two ("`//`") or three slashes ("`///`"),
or they can be embodied within a `@doc` attribute.
The two-slash variant does not propagate the comments to the generated
target, whereas both the three-slash and `@doc` variants do.

That is:

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/attributes.test.fidl" region_tag="doc-with-slashes" %}
```

and

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/attributes.test.fidl" region_tag="doc-with-attributes" %}
```

have the same effect &mdash; one ("`///`") is syntactic sugar for the other.
The text of the comment is
emitted into the generated code, in a manner compatible with the syntax of
the target language.

Note: To be identical, the `@doc` version should be `@doc(" Foo\n")`. Note
the space before the "Foo" and the line-feed "`\n`".

## `@generated_name` {#generated-name}

**USAGE**: `@generated_name("Foo")`

**MEANING**: This attribute is used to override the name that `fidlc` reserves
for any inline layout. This can be useful when a different name is preferred
(for example, to avoid a name conflict with another type).

As an example, the following code leads to a name conflict, because both inline
layouts reserve the same name of `Options`:

```fidl
table StartupConfig {
  1: options table {
    ...
  };
  ...
};

table TeardownConfig {
  2: options table {
    ...
  };
};
```

One way to disambiguate between the two is to manually specify different
generated names:

```fidl
table StartupConfig {
  1: options
  @generated_name("StartupOptions") table {
    ...
  };
  ...
};

table TeardownConfig {
  1: options
  @generated_name("TeardownOptions") table {
    ...
  };
  ...
};
```

## `@no_doc` {#nodoc}

**USAGE**: `@no_doc`

**MEANING**:
This attribute is used to mark libraries that should be skipped by documentation
generation tools. As an example, this attribute is used by generated FIDL
libraries, such as by the [driver bind compiler](/docs/development/drivers/concepts/device_driver_model/driver-binding.md).

## `@selector` {#selector}

**USAGE**: `@selector("`_selector_`")`

**MEANING**:
Allows you to change the hashing basis for the method ordinal, see [RFC-0020].
The _selector_ can be either the original method's name (e.g. `SomeMethod`),
or the [fully qualified name] (e.g. `some.library/SomeProtocol.SomeMethod`).

It can be used to rename a method without breaking ABI compatibility.
For example, if we wish to rename the `Investigate` method to `Experiment`
in the `Science` interface, we can write:

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/attributes.test.fidl" region_tag="selector-simple" %}
```

The attribute can also be used to handle a method moving from one protocol to
another, or to one library to another, or both. For example, consider the method
`Productionize` on the `Org` protocol in the `fuchsia.examples.docs` library
which was originally named `Discover` on the `Area120` protocol, in the
`purple.examples.docs` library:

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples.docs/attributes.test.fidl" region_tag="selector-fq-name" %}
```

## `@transitional` {#transitional}

**USAGE**: `@transitional("`_description_`")`

**MEANING**:
Instructs bindings to generate code that will successfully build, regardless of
whether the method is implemented or not.
[RFC-0021] contains more details.

## `@transport` {#transport}

**USAGE**: `@transport("`_tranportList_`")`

**MEANING**:
Allows you to select a transport.
Provide a comma-separated list of values, selected from:

* `Channel` &mdash; use a [Zircon channel][channel].
* `Syscall` &mdash; transport used to specify that the protocol is used to
  define Zircon syscalls, rather than typical IPC.

The default is `Channel` if none is specified.
If you do specify a value or values, then only those values are used (e.g.,
specifying `@transport("Foo")` disables `Channel` and uses only
`Foo`).

## `@unknown` {#unknown}

**USAGE**: `@unknown`

**MEANING**:
`@unknown` can be placed on an enum member to indicate that this member represents
a specific unknown placeholder. Its purpose is to make it possible to transition a strict enum
that was manually implementing flexible behavior with an extra "unknown" member
to transition into a flexible enum: annotating the member representing unknown
with the `@unknown` attribute before transitioning to flexible ensures that
the `@unknown` member is treated as unknown data, instead of as a
known member. Once usages of the `@unknown` member are removed, the member is
no longer necessary.

<!-- xrefs -->
[channel]: /docs/reference/kernel_objects/channel.md
[RFC-0020]: /docs/contribute/governance/rfcs/0020_interface_ordinal_hashing.md
[RFC-0021]: /docs/contribute/governance/rfcs/0021_soft_transitions_methods_add_remove.md
[RFC-0058]: /docs/contribute/governance/rfcs/0058_deprecated_attribute.md
[identifiers]: /docs/reference/fidl/language/language.md#identifiers
[fully qualified name]: /docs/contribute/governance/rfcs/0043_documentation_comment_format.md#fully-qualified-names
