
# Profile

## NAME

profile - scheduling configuration

## SYNOPSIS

A *profile* allows a set of high level scheduling priorities to be defined and
later applied to one or more threads. Each profile object defines a scheduling
configuration. Once created, the profile can be applied to one or more threads,
which will then adopt those settings.

## DESCRIPTION

Profile objects define a high level scheduling policy that can be applied to
threads. For example, an "audio processing" profile could be created with a high
scheduling priority, and then be applied to threads in media playback jobs.
Alternatively, a "background" profile could be created with a low scheduling
priority, and then be applied to threads in non-interactive jobs.

Profile objects are created with the [`zx_profile_create()`] syscall, passing in
a scheduling configuration. The returned profile may then be applied to one or
more threads using the [`zx_object_set_profile()`] syscall.

Because profiles give significant control of the behaviour of the [kernel
scheduler](/docs/concepts/kernel/kernel_scheduling.md), creating a profile requires the root
resource. Once created, profiles may be delegated freely, however.

Currently, three scheduler parameters are supported:
* `priority`
* `deadline_params`
* `cpu_affinity_mask`

For more details, see [`zx_profile_create()`].

## SYSCALLS

 - [`zx_profile_create()`] - create a new profile object
 - [`zx_object_set_profile()`] - apply a profile to a thread

[`zx_profile_create()`]: /reference/syscalls/profile_create.md
[`zx_object_set_profile()`]: /reference/syscalls/object_set_profile.md
