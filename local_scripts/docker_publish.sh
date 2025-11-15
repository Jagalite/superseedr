docker build -t jagatranvo/superseedr:latest .
docker push jagatranvo/superseedr:latest
docker build --build-arg PRIVATE_BUILD=true -t jagatranvo/superseedr:private .
docker push jagatranvo/superseedr:private
