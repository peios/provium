"""Static guest artifacts and a host runner with its declared overlay."""

def _musl_transition_impl(_settings, _attr):
    return {"//command_line_option:platforms": "//build:musl-platform"}

_musl_transition = transition(
    implementation = _musl_transition_impl,
    inputs = [],
    outputs = ["//command_line_option:platforms"],
)

def _static_agent_impl(ctx):
    return [DefaultInfo(files = ctx.attr.binary[0][DefaultInfo].files)]

static_agent = rule(
    implementation = _static_agent_impl,
    attrs = {
        "binary": attr.label(mandatory = True, cfg = _musl_transition),
        "_allowlist_function_transition": attr.label(default = "@bazel_tools//tools/allowlists/function_transition_allowlist"),
    },
    doc = "Compile the guest agent for musl independently of the host platform.",
)

def _archive_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".cpio.gz")
    ctx.actions.run(
        executable = "/usr/bin/python3",
        arguments = [ctx.file._driver.path, ctx.file.agent.path, output.path, ctx.attr.location],
        inputs = [ctx.file.agent, ctx.file._driver],
        outputs = [output],
        mnemonic = "AgentArchive",
    )
    return [DefaultInfo(files = depset([output]))]

agent_archive = rule(
    implementation = _archive_impl,
    attrs = {
        "agent": attr.label(mandatory = True, allow_single_file = True),
        "location": attr.string(default = "sbin/provium-agent", values = ["sbin/provium-agent", "init"]),
        "_driver": attr.label(default = "//build:agent-archive.py", allow_single_file = True),
    },
    doc = "Produce a deterministic newc archive with the static guest agent.",
)

def _runner_impl(ctx):
    launcher = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(launcher, """#!/bin/bash
set -euo pipefail
root="${RUNFILES_DIR:-$0.runfiles}/%s"
export PROVIUM_OVERLAY="${PROVIUM_OVERLAY:-$root/%s}"
# Bazel starts executable rules in their runfiles directory. Test paths are
# relative to the caller's workspace, as they are for the installed command.
cd "${BUILD_WORKING_DIRECTORY:-$PWD}"
exec "$root/%s" "$@"
""" % (ctx.workspace_name, ctx.file.overlay.short_path, ctx.executable.binary.short_path), is_executable = True)
    return [DefaultInfo(
        executable = launcher,
        runfiles = ctx.runfiles(files = [ctx.executable.binary, ctx.file.overlay]).merge(ctx.attr.binary[DefaultInfo].default_runfiles),
    )]

provium_runner = rule(
    implementation = _runner_impl,
    attrs = {
        "binary": attr.label(mandatory = True, executable = True, cfg = "target"),
        "overlay": attr.label(mandatory = True, allow_single_file = True),
    },
    executable = True,
    doc = "Run the host CLI with the matching declared guest overlay.",
)
