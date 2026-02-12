#!/bin/bash
for i in 1 2 3; do
  echo "Hello from turtle-harbor at $(date) (iteration $i/3)"
  sleep 5
done
echo "Simulating failure!"
exit 1
