from smol import ConnectOptions, Machine, MachineConfig


machine = Machine.create(
    MachineConfig(image="alpine:3.20"),
    ConnectOptions(target="cloud"),  # uses SMOL_CLOUD_TOKEN
)
try:
    # create() waits for ready=True; state "started" only means VM launched.
    result = machine.exec(["echo", "ready for work"]).assert_success()
    print(result.stdout)
finally:
    machine.delete()
