#!/usr/bin/env bash
cd /Users/tobaajiboye/Documents/boundary-led-development
exec ./orchestrate.sh --no-apply "Add a unit test in bld-kernel asserting that a system event at a state with no in-flight effect resolves to Undefined" > .orchestration/run.log 2>&1
