# service-kit

The caller's side of shared services (`protocol/src/contracts/service.rs`) and the one place a
service's answers become text for a model. Part of the shared standard of CortexCode and CortexTerm:
this folder stays identical in both repositories, byte for byte, and a change is made in both in the
same piece of work. CortexCode is its author; CortexTerm copies it.

Use it by path (`cc-service-kit = { path = "../../../service-kit" }`):

1. `client::provider(link, "terminal.command", 1)`: the app on this machine that offers the service
   now, or None (then the caller does the work itself, as before).
2. `client::run(link, &provider, method, params, caller, items, cancel)`: a `run` call. The answer
   is `Ran::Job` (its `state` says done, failed, refused, ...), `Ran::Elsewhere` (the provider is
   gone or restarting: do it yourself) or `Ran::Failed` (report it; doing it elsewhere would fail
   the same way).
3. `client::read(link, &provider, method, params, limit)`: a `read`, `list` or `get`.
4. `text::job`, `text::summary`, `text::moment`: what a model reads. A provider that fills
   `Job.text` is shown as it wrote it.

`Link` is the plugin's connection to its frame (`cc_protocol::Ctx` implements it).
