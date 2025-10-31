#!/bin/bash

# Exit immediately if a command fails
set -e

# 1. Check if a version number was provided as an argument
if [ -z "$1" ]; then
  echo "Error: No version number supplied."
  echo "Usage: ./new_release.sh <version_tag>"
  echo "Example: ./new_release.sh v1.0.0"
  exit 1
fi

# 2. Set variables from the argument
TAG_NAME="$1"
MESSAGE="Release version $TAG_NAME"

# 3. Run the git commands
echo "Creating tag: $TAG_NAME"
git tag -a "$TAG_NAME" -m "$MESSAGE"

echo "Pushing tag $TAG_NAME to origin..."
git push origin "$TAG_NAME"

echo "Successfully created and pushed $TAG_NAME."

# local_scripts/git_tag.sh v0.9.9l
