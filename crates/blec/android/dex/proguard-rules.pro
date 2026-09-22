# Rules for the standalone dex build only; the rules every app needs are in
# ../lib/consumer-rules.pro and reach this build through the :lib dependency.

# Readable stack traces from the shrunk dex; the dex is small either way.
-dontobfuscate
