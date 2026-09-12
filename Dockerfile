FROM python:3.12.3

ENV DEBIAN_FRONTEND=noninteractive

ENV POETRY_NO_INTERACTION=1 \
    POETRY_CACHE_DIR=/tmp/poetry_cache
# POETRY_VIRTUALENVS_IN_PROJECT=1 \
# POETRY_VIRTUALENVS_CREATE=true \

RUN --mount=type=cache,target=~/.cache/pip pip install poetry && poetry config virtualenvs.create false

WORKDIR /app

RUN --mount=type=cache,target=~/.cache/pip pip install wheel Cython

# Sumbernya ada di legacy/, tapi sengaja diletakkan di path container yang sama
# (/app/...) supaya `api.py` yang menulis ke /app/web/sidufh.json dan bind mount
# di docker-compose tetap valid.
COPY ./legacy/rl_string_helper ./rl_string_helper
RUN --mount=type=cache,target=~/.cache/pip pip3 install ./rl_string_helper

COPY ./legacy/database-lib ./database-lib
RUN --mount=type=cache,target=~/.cache/pip pip3 install ./database-lib

COPY ./legacy/medium-parser ./medium-parser
RUN --mount=type=cache,target=~/.cache/pip pip3 install ./medium-parser

COPY ./legacy/web ./web

WORKDIR /app/web

RUN --mount=type=cache,target=/tmp/poetry_cache poetry install --without dev --only main --no-ansi

RUN apt install -y curl

RUN useradd -m freedium
USER freedium

CMD ["python3", "-m", "server", "server"]
