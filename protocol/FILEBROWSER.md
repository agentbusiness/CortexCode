# Shared File Browser contract registration — File Browser 0.1.0

CortexTerm's standalone File Browser 0.1.0 introduces the additive categories `filebrowser@1` and
`filebrowser-native@1` plus the optional `filebrowser` protocol feature. Both applications register
the same categories and typed wire contract so their plugin standard stays aligned. Existing
plugin packaging, shell-module interfaces, mesh transport, model/resource sharing and Terminal
contracts are unchanged. A frame containing these registrations is needed to accept these
categories; this source change does not release or install a new frame.

The plugin implementation and mesh-machine Files tab are CortexTerm-specific. They are not added
to CortexCode's UI or installed here. CortexTerm owns the source at `../CortexTerm/plugins/filebrowser`.

No build, package installation or runtime verification was performed for this registration.
